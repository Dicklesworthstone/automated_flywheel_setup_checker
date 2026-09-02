//! `status` command after real local runs: selection, listing, classification, attempts.

use super::support::*;

#[test]
fn status_reports_classification_and_attempts_after_a_run() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    fx.add_dependency_failure("dep_fail_tool");
    fx.add_unreachable("broken_url_tool");
    let run = fx.run(&["check", "--local", "--format", "jsonl"]);
    assert_eq!(run.status.code(), Some(1));

    let out = fx.run(&["status", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    assert_no_log_noise(&out);
    let doc = json_doc(&out);
    assert_eq!(doc["kind"], "status");
    assert_eq!(doc["run"]["backend"], "local", "run header is persisted and reported");
    assert_eq!(doc["run"]["installer_count"], 3);
    let results = doc["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    let dep = results.iter().find(|r| r["installer_name"] == "dep_fail_tool").unwrap();
    assert_eq!(dep["status"], "failed");
    assert_eq!(dep["error_classification"]["category"], "dependency");
    assert_eq!(dep["sha256_verified"], true);
    assert_eq!(dep["checksum_state"], "verified", "verified even though the installer failed");
    assert_eq!(dep["stderr_tail"], "E: Unable to locate package foo\n");
    let broken = results.iter().find(|r| r["installer_name"] == "broken_url_tool").unwrap();
    assert_eq!(broken["error_classification"]["category"], "network");
    assert_eq!(broken["checksum_state"], "unknown", "download failed before verification");
    assert_eq!(broken["retry_count"], 1);
    assert_eq!(broken["attempts"].as_array().unwrap().len(), 2);
    let good = results.iter().find(|r| r["installer_name"] == "good_tool").unwrap();
    assert!(good["stdout_tail"].as_str().unwrap().contains("mock installer"));
    assert_eq!(doc["summary"]["failed"], 2);
    assert_eq!(doc["summary"]["interrupted"], false);

    let detailed = fx.run(&["status", "--detailed"]);
    let text = stdout(&detailed);
    assert!(text.contains("error: dependency (Dependency, retryable=false"), "{text}");
    assert!(text.contains("attempt 1: failed exit=7"), "{text}");
    assert!(text.contains("attempt 2: failed exit=7"), "{text}");
    assert!(text.contains("broken_url_tool") && text.contains("1 retries"), "{text}");
    assert!(text.contains("checksum: unknown"), "{text}");

    let jsonl = fx.run(&["status", "--format", "jsonl"]);
    let lines = jsonl_lines(&jsonl);
    assert_eq!(lines[0]["kind"], "run");
    assert_eq!(results_of_kind(&lines, "result").len(), 3);
    assert_eq!(lines.last().unwrap()["kind"], "summary");
}

#[test]
fn status_with_no_runs_is_a_single_document() {
    let fx = Fixture::new();
    let out = fx.run(&["status", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    let doc = json_doc(&out);
    assert_eq!(doc["status"], "no_runs");
    let human = fx.run(&["status"]);
    assert!(stdout(&human).contains("No runs found"));
    let list = fx.run(&["status", "--list", "--format", "json"]);
    assert_eq!(json_doc(&list)["runs"].as_array().unwrap().len(), 0);
}

#[test]
fn status_lists_and_selects_runs_that_started_in_the_same_second() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    // Two runs back to back: both must survive (millisecond names + run id suffix).
    let a = jsonl_lines(&fx.run(&["check", "--local", "--format", "jsonl"]));
    let b = jsonl_lines(&fx.run(&["check", "--local", "--format", "jsonl"]));
    let run_a = a[0]["run_id"].as_str().unwrap().to_string();
    let run_b = b[0]["run_id"].as_str().unwrap().to_string();
    assert_ne!(run_a, run_b);

    let list = fx.run(&["status", "--list", "--format", "json"]);
    let doc = json_doc(&list);
    let runs = doc["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["run_id"], run_b, "newest first");
    assert_eq!(runs[1]["run_id"], run_a);
    assert_eq!(runs[0]["passed"], 1);

    let human = fx.run(&["status", "--list"]);
    let text = stdout(&human);
    assert!(text.contains(&run_b[..8]) && text.contains(&run_a[..8]), "{text}");

    // Default status is the newest run; --run selects by prefix.
    let newest = json_doc(&fx.run(&["status", "--format", "json"]));
    assert_eq!(newest["run"]["run_id"], run_b);
    let selected = json_doc(&fx.run(&["status", "--run", &run_a[..8], "--format", "json"]));
    assert_eq!(selected["run"]["run_id"], run_a);
    let last = json_doc(&fx.run(&["status", "--run", "last", "--format", "json"]));
    assert_eq!(last["run"]["run_id"], run_b);
    let missing = fx.run(&["status", "--run", "zzzzzzzz"]);
    assert!(!missing.status.success());
}

#[test]
fn results_retention_prunes_only_old_result_files() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    // Fixture config carries [general] with acfs_repo; append a retention of 2.
    let cfg = std::fs::read_to_string(&fx.config).unwrap();
    std::fs::write(&fx.config, cfg.replace("log_level = \"info\"", "log_level = \"info\"\nresults_retention = 2")).unwrap();
    for _ in 0..4 {
        fx.run(&["check", "--local", "--format", "jsonl"]);
    }
    let results_dir = fx.home.join(".local/share/afsc/results");
    std::fs::write(results_dir.join("notes.txt"), "keep").unwrap();
    fx.run(&["check", "--local", "--format", "jsonl"]);
    let files: Vec<_> = std::fs::read_dir(&results_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    let result_files = files.iter().filter(|f| f.starts_with("results_")).count();
    assert_eq!(result_files, 2, "{files:?}");
    assert!(files.contains(&"notes.txt".to_string()));
}

#[test]
fn status_prometheus_reflects_the_last_run() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    fx.add_dependency_failure("dep_fail_tool");
    fx.run(&["check", "--local", "--format", "jsonl"]);
    let out = fx.run(&["status", "--format", "prometheus"]);
    let text = stdout(&out);
    assert!(text.contains("afsc_tests_total_24h 2"), "{text}");
    assert!(text.contains("afsc_successful_tests_24h 1"), "{text}");
}

#[test]
fn data_dir_flag_relocates_results_and_metrics() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    let data = fx.root.path().join("elsewhere");
    let data_s = data.to_string_lossy().to_string();
    let out = fx.run(&["--data-dir", &data_s, "check", "--local", "--format", "jsonl"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(data.join("results").exists());
    assert!(data.join("metrics.json").exists());
    assert!(!fx.home.join(".local/share/afsc").exists(), "default data dir untouched");
    let status = json_doc(&fx.run(&["--data-dir", &data_s, "status", "--format", "json"]));
    assert_eq!(status["results"].as_array().unwrap().len(), 1);
}

#[test]
fn structured_event_log_records_each_run_and_prunes_old_files() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    fx.add_dependency_failure("dep_fail_tool");
    let log_dir = fx.home.join(".local/share/afsc/logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let stale = log_dir.join("checker_20200101.jsonl");
    std::fs::write(&stale, "{}\n").unwrap();

    let first = jsonl_lines(&fx.run(&["check", "--local", "--format", "jsonl"]));
    let run_id = first[0]["run_id"].as_str().unwrap().to_string();
    assert!(!stale.exists(), "logs older than the retention are pruned");
    let files: Vec<_> = std::fs::read_dir(&log_dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].starts_with("checker_") && files[0].ends_with(".jsonl"));
    let text = std::fs::read_to_string(log_dir.join(&files[0])).unwrap();
    let events: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let mine: Vec<&serde_json::Value> = events.iter().filter(|e| e["correlation_id"] == run_id).collect();
    assert_eq!(mine.iter().filter(|e| e["event"] == "run_started").count(), 1);
    assert_eq!(mine.iter().filter(|e| e["event"] == "installer_finished").count(), 2);
    let dep = mine.iter().find(|e| e["installer"] == "dep_fail_tool").unwrap();
    assert_eq!(dep["data"]["category"], "dependency");
    assert_eq!(dep["data"]["status"], "failed");
    let finished = mine.iter().find(|e| e["event"] == "run_finished").unwrap();
    assert_eq!(finished["data"]["failed"], 1);
    assert_eq!(finished["data"]["interrupted"], false);

    // A second run appends to the same day's file.
    fx.run(&["check", "--local", "--format", "jsonl"]);
    let text = std::fs::read_to_string(log_dir.join(&files[0])).unwrap();
    assert_eq!(text.lines().filter(|l| l.contains("run_started")).count(), 2);
}
