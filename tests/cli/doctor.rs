//! `doctor`: environment diagnosis with fix hints; run-header reproducibility fields.

use super::support::*;

fn check<'a>(doc: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no check {name}: {doc}"))
}

#[test]
fn doctor_reports_each_check_with_hints_and_exit_code() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    // Fresh fixture, no runs yet, Docker skipped: warnings but nothing failing.
    let out = fx.run(&["doctor", "--local", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_no_log_noise(&out);
    let doc = json_doc(&out);
    assert_eq!(doc["kind"], "doctor");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(check(&doc, "docker")["status"], "skip");
    assert_eq!(check(&doc, "config")["status"], "pass");
    assert_eq!(check(&doc, "acfs_repo")["status"], "pass");
    assert!(check(&doc, "acfs_repo")["detail"].as_str().unwrap().contains("1 installers"));
    assert_eq!(check(&doc, "acfs_cross_check")["status"], "skip", "fixture is not a full checkout");
    assert_eq!(check(&doc, "data_dir")["status"], "pass");
    assert_eq!(check(&doc, "log_dir")["status"], "pass");
    assert_eq!(check(&doc, "last_run")["status"], "warn");
    assert!(check(&doc, "last_run")["hint"].as_str().unwrap().contains("check"));
    assert_eq!(check(&doc, "notifications")["status"], "skip");
    assert_eq!(check(&doc, "validate")["status"], "skip");
    assert!(doc["checks"].as_array().unwrap().iter().any(|c| c["name"] == "disk"));
    assert_eq!(doc["failed"], 0);

    // After a run and a hash check the warnings clear.
    fx.run(&["check", "--local", "--format", "jsonl"]);
    fx.run(&["validate", "--check-hashes"]);
    let doc = json_doc(&fx.run(&["doctor", "--local", "--format", "json"]));
    assert_eq!(check(&doc, "last_run")["status"], "pass");
    assert_eq!(check(&doc, "validate")["status"], "pass");
    let human = fx.run(&["doctor", "--local"]);
    assert!(stdout(&human).contains("ready"), "{}", stdout(&human));
    let jsonl = jsonl_lines(&fx.run(&["doctor", "--local", "--format", "jsonl"]));
    assert!(jsonl.iter().any(|l| l["kind"] == "doctor_check" && l["name"] == "acfs_repo"));
    assert_eq!(jsonl.last().unwrap()["kind"], "doctor_summary");

    // Unknown config keys warn; a missing ACFS repo fails with exit 3 and a hint.
    fx.add_config_toml("[docker]\nimgae = \"typo\"\n");
    let doc = json_doc(&fx.run(&["doctor", "--local", "--format", "json"]));
    assert_eq!(check(&doc, "config")["status"], "warn");
    assert!(check(&doc, "config")["detail"].as_str().unwrap().contains("docker.imgae"));
    let out =
        fx.run(&["--acfs-repo", "/nonexistent/acfs", "doctor", "--local", "--format", "json"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let doc = json_doc(&out);
    assert_eq!(check(&doc, "acfs_repo")["status"], "fail");
    assert!(check(&doc, "acfs_repo")["hint"].as_str().unwrap().contains("acfs_repo"));
}

#[test]
fn doctor_flags_missing_notification_secrets_and_claude_when_needed() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    fx.add_config_toml("[notifications]\nenabled = true\nslack_webhook_env = \"AFSC_DOCTOR_TEST_HOOK\"\n[remediation]\nmode = \"advisory\"\n");
    let doc = json_doc(&fx.run_with(
        &["doctor", "--local", "--format", "json"],
        &[],
        &["AFSC_DOCTOR_TEST_HOOK"],
    ));
    assert_eq!(check(&doc, "notifications")["status"], "warn");
    assert!(check(&doc, "notifications")["detail"]
        .as_str()
        .unwrap()
        .contains("AFSC_DOCTOR_TEST_HOOK"));
    let claude = check(&doc, "claude");
    assert!(matches!(claude["status"].as_str().unwrap(), "pass" | "fail"), "{claude}");
    let doc = json_doc(&fx.run_with(
        &["doctor", "--local", "--format", "json"],
        &[("AFSC_DOCTOR_TEST_HOOK", "https://hooks.example/x")],
        &[],
    ));
    assert_eq!(check(&doc, "notifications")["status"], "pass");
}

#[test]
fn run_header_carries_reproducibility_fields() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    let lines = jsonl_lines(&fx.run_with(
        &["check", "--local", "--format", "jsonl"],
        &[("AFSC_ALLOW_LOCAL", "1"), ("AFSC_EXECUTION_RUN_DEADLINE_SECONDS", "600")],
        &[],
    ));
    let header = &lines[0];
    assert_eq!(header["kind"], "run");
    assert_eq!(header["schema_version"], 1);
    assert_eq!(header["deadline_seconds"], 600);
    assert_eq!(header["run_as_root"], false);
    assert!(header["image_id"].is_null(), "local runs have no image");
    assert_eq!(header["environment"]["os"], "linux");
    assert!(header["environment"]["host"].as_str().is_some_and(|h| !h.is_empty()));
    assert_eq!(header["environment"]["tool_version"], env!("CARGO_PKG_VERSION"));
    // Every JSONL line carries the schema version.
    assert!(lines.iter().all(|l| l["schema_version"] == 1), "{lines:?}");
    let status = json_doc(&fx.run(&["status", "--format", "json"]));
    assert_eq!(status["run"]["deadline_seconds"], 600);
    assert_eq!(status["run"]["environment"]["os"], "linux");
}
