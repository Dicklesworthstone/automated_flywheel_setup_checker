//! `status` command after real local runs.

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
    let results = doc["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    let dep = results.iter().find(|r| r["installer_name"] == "dep_fail_tool").unwrap();
    assert_eq!(dep["status"], "failed");
    assert_eq!(dep["error_classification"]["category"], "dependency");
    assert_eq!(dep["sha256_verified"], true);
    let broken = results.iter().find(|r| r["installer_name"] == "broken_url_tool").unwrap();
    assert_eq!(broken["error_classification"]["category"], "network");
    assert_eq!(broken["retry_count"], 1);
    assert_eq!(broken["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(doc["summary"]["failed"], 2);

    let detailed = fx.run(&["status", "--detailed"]);
    let text = stdout(&detailed);
    assert!(text.contains("error: dependency (Dependency, retryable=false"), "{text}");
    assert!(text.contains("attempt 1: failed exit=7"), "{text}");
    assert!(text.contains("attempt 2: failed exit=7"), "{text}");
    assert!(text.contains("broken_url_tool") && text.contains("1 retries"), "{text}");

    let jsonl = fx.run(&["status", "--format", "jsonl"]);
    let lines = jsonl_lines(&jsonl);
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
