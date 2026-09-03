//! Run history: `status --history/--diff`, `--format markdown`, `check --failed-from`,
//! longest-first ordering and the whole-run deadline.

use super::support::*;

#[test]
fn history_diff_markdown_and_failed_from_over_two_runs() {
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    fx.add_pass("good_tool");
    fx.add_flaky("flaky_tool", 1); // fails once (no retries), then passes

    let run1 = jsonl_lines(&fx.run(&["check", "--local", "--format", "jsonl"]));
    let id1 = run1[0]["run_id"].as_str().unwrap().to_string();
    assert_eq!(find_result(&run1, "flaky_tool")["status"], "failed");

    // Rerun only the failure from run 1: exactly one installer runs and it is recorded as requested.
    let rerun = fx.run(&["check", "--local", "--format", "jsonl", "--failed-from", &id1[..8]]);
    assert_eq!(rerun.status.code(), Some(0), "{}", stderr(&rerun));
    let lines = jsonl_lines(&rerun);
    let id2 = lines[0]["run_id"].as_str().unwrap().to_string();
    assert_eq!(lines[0]["installers_requested"], serde_json::json!(["flaky_tool"]));
    assert_eq!(results_of_kind(&lines, "result").len(), 1);
    assert_eq!(find_result(&lines, "flaky_tool")["status"], "passed");

    // Nothing to rerun once the last run is clean.
    let nothing = fx.run(&["check", "--local", "--format", "json", "--failed-from", "last"]);
    assert_eq!(nothing.status.code(), Some(0));
    assert_eq!(json_doc(&nothing)["status"], "nothing_to_rerun");
    let unknown = fx.run(&["check", "--local", "--failed-from", "zzzzzzzz"]);
    assert_eq!(unknown.status.code(), Some(2));

    // Timeline (oldest first) with assessment.
    let hist = json_doc(&fx.run(&["status", "--history", "flaky_tool", "--format", "json"]));
    assert_eq!(hist["kind"], "history");
    let entries = hist["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["run_id"], id1);
    assert_eq!(entries[0]["status"], "failed");
    assert_eq!(entries[0]["category"], "network");
    assert_eq!(entries[1]["status"], "passed");
    assert_eq!(hist["assessment"]["trials"], 2);
    assert_eq!(hist["assessment"]["flaky"], false, "too few trials");
    let limited = json_doc(&fx.run(&[
        "status",
        "--history",
        "flaky_tool",
        "--last",
        "1",
        "--format",
        "json",
    ]));
    assert_eq!(limited["entries"].as_array().unwrap().len(), 1);
    let human = fx.run(&["status", "--history", "flaky_tool"]);
    assert!(stdout(&human).contains("flaky_tool: 2 run(s)"), "{}", stdout(&human));
    let missing = fx.run(&["status", "--history", "nope"]);
    assert_eq!(missing.status.code(), Some(2));
    let jsonl = jsonl_lines(&fx.run(&["status", "--history", "flaky_tool", "--format", "jsonl"]));
    assert_eq!(jsonl.iter().filter(|l| l["kind"] == "history_entry").count(), 2);
    assert_eq!(jsonl.last().unwrap()["kind"], "assessment");

    // Diff between the two runs: flaky_tool recovered, good_tool absent from run 2 => removed.
    let diff = json_doc(&fx.run(&["status", "--diff", &id1[..8], &id2[..8], "--format", "json"]));
    assert_eq!(diff["kind"], "diff");
    let changes = diff["changes"].as_array().unwrap();
    let flaky = changes.iter().find(|c| c["installer"] == "flaky_tool").unwrap();
    assert_eq!(flaky["change"], "recovered");
    assert_eq!(flaky["before"], "failed");
    assert_eq!(flaky["after"], "passed");
    let good = changes.iter().find(|c| c["installer"] == "good_tool").unwrap();
    assert_eq!(good["change"], "removed");
    let human = fx.run(&["status", "--diff", "last", &id1[..8]]);
    assert!(stdout(&human).contains("regressed"), "{}", stdout(&human));

    // Markdown for the run, the timeline and the diff.
    let md = stdout(&fx.run(&["status", "--format", "markdown"]));
    assert!(md.starts_with("## AFSC run `"), "{md}");
    assert!(md.contains("| flaky_tool | ✅ passed |"), "{md}");
    let md = stdout(&fx.run(&["status", "--history", "flaky_tool", "--format", "markdown"]));
    assert!(md.contains("## flaky_tool — 2 runs"), "{md}");
    let md = stdout(&fx.run(&["status", "--diff", &id1[..8], "last", "--format", "markdown"]));
    assert!(md.contains("| flaky_tool | recovered |"), "{md}");
    // Markdown is a status-only format.
    let bad = fx.run(&["list", "--format", "markdown"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn longest_first_ordering_uses_history_and_name_order_is_alphabetical() {
    let mut fx = Fixture::new();
    fx.add_sleeper("slow_tool", 1);
    fx.add_pass("quick_tool");
    // No history: unknown durations are ordered by name.
    let dry = json_doc(&fx.run(&["check", "--local", "--dry-run", "--format", "json"]));
    let names = |doc: &serde_json::Value| -> Vec<String> {
        doc["installers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(names(&dry), vec!["quick_tool", "slow_tool"]);

    let run = fx.run(&["check", "--local", "--format", "jsonl"]);
    assert_eq!(run.status.code(), Some(0), "{}", stderr(&run));

    // With history the 1 s sleeper goes first (LPT); `order = name` restores alphabetical order.
    let dry = json_doc(&fx.run(&["check", "--local", "--dry-run", "--format", "json"]));
    assert_eq!(names(&dry), vec!["slow_tool", "quick_tool"]);
    let by_name = json_doc(&fx.run_with(
        &["check", "--local", "--dry-run", "--format", "json"],
        &[("AFSC_EXECUTION_ORDER", "name")],
        &[],
    ));
    assert_eq!(names(&by_name), vec!["quick_tool", "slow_tool"]);
    let manifest = json_doc(&fx.run_with(
        &["check", "--local", "--dry-run", "--format", "json"],
        &[("AFSC_EXECUTION_ORDER", "manifest")],
        &[],
    ));
    assert_eq!(
        names(&manifest),
        vec!["slow_tool", "quick_tool"],
        "checksums.yaml order (insertion)"
    );
}

#[test]
fn run_deadline_cancels_remaining_work_without_failing_the_run() {
    let mut fx = Fixture::new();
    fx.add_pass("a_quick_tool");
    fx.add_sleeper("b_slow_tool", 15);
    let start = std::time::Instant::now();
    let out = fx.run_with(
        &["check", "--local", "--format", "jsonl"],
        &[
            ("AFSC_ALLOW_LOCAL", "1"),
            ("AFSC_EXECUTION_RUN_DEADLINE_SECONDS", "2"),
            ("AFSC_EXECUTION_ORDER", "name"),
        ],
        &[],
    );
    assert!(start.elapsed().as_secs() < 12, "deadline stopped the sleeper: {:?}", start.elapsed());
    assert_eq!(
        out.status.code(),
        Some(0),
        "deadline is not an installer failure: {}",
        stderr(&out)
    );
    let lines = jsonl_lines(&out);
    assert_eq!(find_result(&lines, "a_quick_tool")["status"], "passed");
    let slow = find_result(&lines, "b_slow_tool");
    assert_eq!(slow["status"], "cancelled");
    assert_eq!(slow["error"]["category"], "cancelled");
    let summary = lines.last().unwrap();
    assert_eq!(summary["kind"], "summary");
    assert_eq!(summary["deadline_exceeded"], true);
    assert_eq!(summary["cancelled"], 1);
    assert_eq!(summary["exit_code"], 0);
    assert!(stderr(&out).contains("deadline"), "{}", stderr(&out));
    // The persisted run reports the cancellation too.
    let status = json_doc(&fx.run(&["status", "--format", "json"]));
    let slow = status["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["installer_name"] == "b_slow_tool")
        .unwrap();
    assert_eq!(slow["status"], "cancelled");
}
