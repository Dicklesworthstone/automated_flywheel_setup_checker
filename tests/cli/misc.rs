//! validate, list, classify-error, config: stdout purity and basic behavior.

use super::support::*;

#[test]
fn validate_json_is_one_document() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    let out = fx.run(&["-vvv", "validate", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    assert_no_log_noise(&out);
    let doc = json_doc(&out);
    assert_eq!(doc["kind"], "validate");
    assert_eq!(doc["format"]["valid"], true);
    assert_eq!(doc["exit_code"], 0);
}

#[test]
fn validate_jsonl_has_kinds() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    let out = fx.run(&["validate", "--format", "jsonl"]);
    let lines = jsonl_lines(&out);
    assert_eq!(lines[0]["kind"], "format");
    assert_eq!(lines.last().unwrap()["kind"], "summary");
}

#[test]
fn validate_reports_invalid_url_with_exit_code() {
    let mut fx = Fixture::new();
    fx.add_entry("bad", "not a url", &"0".repeat(64));
    let out = fx.run(&["validate", "--format", "json"]);
    assert_eq!(out.status.code(), Some(2), "format errors are configuration errors");
    let doc = json_doc(&out);
    assert_eq!(doc["format"]["valid"], false);
    assert!(doc["format"]["errors"][0].as_str().unwrap().contains("Invalid URL"));
}

#[test]
fn list_jsonl_lines_carry_kind_and_are_sorted() {
    let mut fx = Fixture::new();
    fx.add_pass("zeta");
    fx.add_pass("alpha");
    let out = fx.run(&["list", "--format", "jsonl"]);
    let lines = jsonl_lines(&out);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["kind"], "installer");
    assert_eq!(lines[0]["name"], "alpha");
    assert_eq!(lines[1]["name"], "zeta");
    let json = fx.run(&["list", "--format", "json"]);
    assert_eq!(json_doc(&json).as_array().unwrap().len(), 2);
}

#[test]
fn classify_error_explain_names_the_pattern() {
    let fx = Fixture::new();
    let out = fx.run(&[
        "classify-error",
        "--stderr",
        "E: Unable to locate package foo",
        "--exit-code",
        "100",
        "--explain",
        "--format",
        "json",
    ]);
    let doc = json_doc(&out);
    assert_eq!(doc["kind"], "classification");
    assert_eq!(doc["category"], "dependency");
    assert_eq!(doc["explain"]["category"], "dependency");
    assert!(doc["explain"]["pattern"].as_str().unwrap().contains("unable to locate package"));

    let human = fx.run(&["classify-error", "--stderr", "Test timed out after 300s", "--exit-code", "-1", "--explain"]);
    let text = stdout(&human);
    assert!(text.contains("Category: timeout"), "{text}");
    assert!(text.contains("Matched: timeout"), "{text}");
}

#[test]
fn config_log_level_from_file_applies_without_verbose_flag() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    // Fixture config sets log_level = "info": an INFO line must appear on stderr without -v.
    let out = fx.run(&["check", "--local", "--format", "jsonl"]);
    assert!(stderr(&out).contains("Starting installer test"), "{}", stderr(&out));
    assert_no_log_noise(&out);
}

#[test]
fn log_format_json_emits_json_lines_on_stderr() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    let out = fx.run(&["--log-format", "json", "check", "--local", "--format", "jsonl"]);
    let err = stderr(&out);
    let first = err.lines().find(|l| l.starts_with('{')).expect("a JSON log line");
    let v: serde_json::Value = serde_json::from_str(first).unwrap();
    assert!(v.get("timestamp").is_some() || v.get("fields").is_some(), "{first}");
    assert_no_log_noise(&out);
}
