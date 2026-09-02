//! `check` command: stdout purity, classification, attempt history, config-driven execution.

use super::support::*;
use std::time::Instant;

#[test]
fn check_local_jsonl_is_pure_and_every_failure_is_classified() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    fx.add_dependency_failure("dep_fail_tool");
    fx.add_installer_with_wrong_hash("wrong_sum_tool", "#!/bin/bash\nexit 0\n");
    fx.add_root_refusal("root_tool");
    fx.add_unreachable("broken_url_tool");

    let out = fx.run(&["-vvv", "check", "--local", "--format", "jsonl"]);
    assert_eq!(out.status.code(), Some(1), "failures must exit 1");
    assert_no_log_noise(&out);

    let lines = jsonl_lines(&out);
    assert_eq!(lines.first().unwrap()["kind"], "run", "first line is the run header");
    assert_eq!(lines.last().unwrap()["kind"], "summary", "last line is the summary");
    assert_eq!(lines.first().unwrap()["schema_version"], 1);
    assert_eq!(lines.first().unwrap()["backend"], "local");
    assert_eq!(lines.first().unwrap()["installer_count"], 5);

    let results = results_of_kind(&lines, "result");
    assert_eq!(results.len(), 5);

    let good = find_result(&lines, "good_tool");
    assert_eq!(good["status"], "Passed");
    assert!(good["error"].is_null());
    assert_eq!(good["checksum_result"]["matches"], true);

    let dep = find_result(&lines, "dep_fail_tool");
    assert_eq!(dep["status"], "Failed");
    assert_eq!(dep["exit_code"], 100);
    assert_eq!(dep["error"]["category"], "dependency");
    assert_eq!(dep["checksum_result"]["matches"], true, "verified before it failed");

    let wrong = find_result(&lines, "wrong_sum_tool");
    assert_eq!(wrong["exit_code"], 99);
    assert_eq!(wrong["error"]["category"], "checksum_mismatch");
    assert_eq!(wrong["checksum_result"]["matches"], false);
    assert_eq!(wrong["attempts"].as_array().unwrap().len(), 1, "checksum mismatch never retries");

    let root = find_result(&lines, "root_tool");
    assert_eq!(root["error"]["category"], "permission", "stdout-only refusal is classified");

    let broken = find_result(&lines, "broken_url_tool");
    assert_eq!(broken["error"]["category"], "network");
    assert!(broken["error"]["retryable"].as_bool().unwrap());
    let attempts = broken["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2, "retry_transient = 1 means two attempts");
    assert!(attempts[1]["waited_before_ms"].as_u64().unwrap() >= 1000);

    let summary = lines.last().unwrap();
    assert_eq!(summary["total"], 5);
    assert_eq!(summary["passed"], 1);
    assert_eq!(summary["failed"], 4);
    assert_eq!(summary["exit_code"], 1);
}

#[test]
fn check_json_is_one_document_even_with_failures_and_verbose_logs() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    fx.add_dependency_failure("dep_fail_tool");

    let out = fx.run(&["-vvv", "check", "--local", "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_no_log_noise(&out);
    let doc = json_doc(&out);
    assert_eq!(doc["kind"], "check");
    assert_eq!(doc["run"]["kind"], "run");
    assert_eq!(doc["summary"]["failed"], 1);
    let results = doc["results"].as_array().unwrap();
    let dep = results.iter().find(|r| r["installer_name"] == "dep_fail_tool").unwrap();
    assert_eq!(dep["error"]["category"], "dependency");
    assert!(stderr(&out).contains("Installer test failed"), "logs went to stderr");
}

#[test]
fn check_stdout_is_identical_with_and_without_verbose() {
    let mut fx = Fixture::new();
    fx.add_dependency_failure("dep_fail_tool");
    let quiet = fx.run(&["check", "--local", "--format", "jsonl"]);
    let loud = fx.run(&["-vvv", "check", "--local", "--format", "jsonl"]);
    let strip = |s: String| -> Vec<String> {
        // run_id, timestamps, and durations differ between runs; compare structure only.
        s.lines()
            .map(|l| {
                let mut v: serde_json::Value = serde_json::from_str(l).unwrap();
                for k in [
                    "run_id",
                    "started_at",
                    "finished_at",
                    "duration",
                    "duration_ms",
                    "last_attempt_ms",
                    "attempts",
                    "retries",
                    "duration_total_ms",
                    "checksum_result",
                ] {
                    v.as_object_mut().map(|m| m.remove(k));
                }
                v.to_string()
            })
            .collect()
    };
    assert_eq!(strip(stdout(&quiet)), strip(stdout(&loud)));
}

#[test]
fn check_human_output_shows_category_and_suggestion() {
    let mut fx = Fixture::new();
    fx.add_dependency_failure("dep_fail_tool");
    let out = fx.run(&["check", "--local"]);
    let text = stdout(&out);
    assert!(text.contains("\u{2717} dep_fail_tool"), "{text}");
    assert!(text.contains("error: dependency (Dependency, retryable=false"), "{text}");
    assert!(text.contains("suggestion: Install missing dependencies"), "{text}");
    assert!(text.contains("Results: 0 passed, 1 failed out of 1 total"));

    let quiet = fx.run(&["--quiet", "check", "--local"]);
    let text = stdout(&quiet);
    assert!(!text.contains("dep_fail_tool ("), "quiet suppresses per-result lines:\n{text}");
    assert!(text.contains("Results: 0 passed, 1 failed"));
}

#[test]
fn check_records_full_attempt_history_for_flaky_installer() {
    let mut fx = Fixture::new();
    fx.set_execution(1, 2, false);
    fx.add_flaky("flaky_tool", 1);

    let out = fx.run(&["check", "--local", "--format", "jsonl"]);
    assert_eq!(out.status.code(), Some(0), "recovers on the second attempt");
    let lines = jsonl_lines(&out);
    let r = find_result(&lines, "flaky_tool");
    assert_eq!(r["status"], "Passed");
    assert_eq!(r["attempt"], 2);
    assert_eq!(r["max_attempts"], 3);
    let attempts = r["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["status"], "Failed");
    assert_eq!(attempts[0]["exit_code"], 7);
    assert!(attempts[0]["stderr_tail"].as_str().unwrap().contains("Connection refused"));
    assert_eq!(attempts[1]["status"], "Passed");
    assert!(attempts[1]["waited_before_ms"].as_u64().unwrap() >= 1000);
    assert_eq!(r["retries"].as_array().unwrap().len(), 1);
    let total = r["duration_ms"].as_u64().unwrap();
    let last = r["last_attempt_ms"].as_u64().unwrap();
    let waited = attempts[1]["waited_before_ms"].as_u64().unwrap();
    let first = attempts[0]["duration_ms"].as_u64().unwrap();
    assert_eq!(last, attempts[1]["duration_ms"].as_u64().unwrap(), "last_attempt_ms is the final attempt");
    assert!(total >= first + waited + last, "total {total} must include both attempts and the wait");
    assert!(total >= 1000, "total duration includes the backoff wait");
}

#[test]
fn check_zero_retries_means_single_attempt() {
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    fx.add_unreachable("broken_url_tool");
    let out = fx.run(&["check", "--local", "--format", "jsonl"]);
    let lines = jsonl_lines(&out);
    let r = find_result(&lines, "broken_url_tool");
    assert_eq!(r["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(r["max_attempts"], 1);
    assert_eq!(lines[0]["retries"], 0);
}

#[test]
fn check_honors_parallel_from_config() {
    let mut fx = Fixture::new();
    fx.set_execution(3, 0, false);
    fx.add_sleeper("s1", 1);
    fx.add_sleeper("s2", 1);
    fx.add_sleeper("s3", 1);
    let start = Instant::now();
    let out = fx.run(&["check", "--local", "--format", "jsonl"]);
    let elapsed = start.elapsed();
    assert_eq!(out.status.code(), Some(0));
    let lines = jsonl_lines(&out);
    assert_eq!(lines[0]["parallel"], 3);
    assert!(elapsed.as_secs_f64() < 2.5, "three 1 s installers ran concurrently: {elapsed:?}");

    let cli_override = fx.run(&["check", "--local", "--format", "jsonl", "--parallel", "1"]);
    assert_eq!(jsonl_lines(&cli_override)[0]["parallel"], 1);
}

#[test]
fn parallel_fail_fast_cancels_in_flight_and_skips_queued() {
    let mut fx = Fixture::new();
    fx.set_execution(3, 0, false);
    // Sorted by name: a_fail runs first and fails fast; slow ones are in flight or queued.
    fx.add_installer("a_fail", "#!/bin/bash\nsleep 1\necho 'E: Unable to locate package foo' >&2\nexit 100\n");
    fx.add_sleeper("b_slow", 20);
    fx.add_sleeper("c_slow", 20);
    fx.add_sleeper("d_queued", 20);
    let start = Instant::now();
    let out = fx.run(&["check", "--local", "--fail-fast", "--format", "jsonl"]);
    let elapsed = start.elapsed();
    assert_eq!(out.status.code(), Some(1));
    assert!(elapsed.as_secs() < 12, "fail-fast must stop in-flight installers: {elapsed:?}");
    let lines = jsonl_lines(&out);
    assert_eq!(find_result(&lines, "a_fail")["status"], "Failed");
    for name in ["b_slow", "c_slow", "d_queued"] {
        let s = find_result(&lines, name)["status"].as_str().unwrap().to_string();
        assert!(s == "Cancelled" || s == "Skipped", "{name}: {s}");
    }
    let summary = lines.last().unwrap();
    assert_eq!(summary["failed"], 4);
    assert!(summary["cancelled"].as_u64().unwrap() + summary["skipped"].as_u64().unwrap() == 3);
    assert_eq!(summary["interrupted"], false, "fail-fast is not an interruption");
}

#[test]
fn check_honors_fail_fast_from_config_in_sequential_mode() {
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, true);
    fx.add_dependency_failure("a_fail");
    fx.add_pass("b_pass");
    let out = fx.run(&["check", "--local", "--format", "jsonl"]);
    let lines = jsonl_lines(&out);
    assert_eq!(lines[0]["fail_fast"], true);
    assert_eq!(results_of_kind(&lines, "result").len(), 1, "stops after the first failure");
}

#[test]
fn check_dry_run_json_lists_installers() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    let out = fx.run(&["check", "--local", "--dry-run", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    let doc = json_doc(&out);
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["installers"][0], "good_tool");
    assert_eq!(doc["backend"], "local");
}

#[test]
fn check_missing_checksums_is_a_config_error_exit_2() {
    let fx = Fixture::new();
    std::fs::remove_file(fx.acfs.join("checksums.yaml")).unwrap();
    let out = fx.run(&["check", "--local"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("checksums.yaml not found"));
}

#[test]
fn docker_unreachable_is_an_infrastructure_error_exit_3() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("automated_flywheel_setup_checker");
    let out = cmd
        .env("HOME", &fx.home)
        .env("DOCKER_HOST", "unix:///nonexistent/afsc-test/docker.sock")
        .arg("--config")
        .arg(&fx.config)
        .args(["check", "good_tool"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("Docker daemon unreachable"), "{err}");
    assert!(err.contains("--local"), "{err}");
}

#[cfg(unix)]
#[test]
fn sigterm_cancels_in_flight_installers_and_persists_an_interrupted_run() {
    use std::time::{Duration, Instant};
    let mut fx = Fixture::new();
    fx.set_execution(2, 0, false);
    fx.add_sleeper("slow_a", 30);
    fx.add_sleeper("slow_b", 30);
    fx.add_pass("queued_c");

    let bin = assert_cmd::cargo::cargo_bin!("automated_flywheel_setup_checker").to_path_buf();
    let mut child = std::process::Command::new(bin)
        .env("HOME", &fx.home)
        .arg("--config")
        .arg(&fx.config)
        .args(["check", "--local", "--format", "jsonl"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let start = Instant::now();
    let status = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let out = child.wait_with_output().unwrap();
    assert!(start.elapsed() < Duration::from_secs(10), "cancellation must be prompt");
    assert_eq!(out.status.code(), Some(143), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let lines = jsonl_lines(&out);
    let summary = lines.last().unwrap();
    assert_eq!(summary["kind"], "summary");
    assert_eq!(summary["interrupted"], true);
    assert!(summary["cancelled"].as_u64().unwrap() >= 2, "{summary}");
    for r in results_of_kind(&lines, "result") {
        assert!(matches!(r["status"].as_str().unwrap(), "Cancelled" | "Skipped"), "{r}");
        if r["status"] == "Cancelled" {
            assert_eq!(r["error"]["category"], "cancelled");
        }
    }

    // Persisted too.
    let status_doc = json_doc(&fx.run(&["status", "--format", "json"]));
    assert_eq!(status_doc["summary"]["interrupted"], true);
}
