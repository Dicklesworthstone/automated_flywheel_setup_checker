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
    fx.add_pass("skipped_one");
    fx.add_override("skipped_one", "skip = true\nskip_reason = \"broken upstream\"");
    fx.add_override("zeta", "args = [\"--quiet\"]");
    let out = fx.run(&["list", "--format", "jsonl"]);
    let lines = jsonl_lines(&out);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["kind"], "installer");
    assert_eq!(lines[0]["name"], "alpha");
    assert_eq!(lines[1]["name"], "skipped_one");
    assert_eq!(lines[1]["skip_reason"], "broken upstream");
    assert_eq!(lines[2]["name"], "zeta");
    assert_eq!(lines[2]["args"], serde_json::json!(["--quiet"]));
    let json = fx.run(&["list", "--format", "json"]);
    assert_eq!(json_doc(&json).as_array().unwrap().len(), 3);
    let runnable = jsonl_lines(&fx.run(&["list", "--runnable", "--format", "jsonl"]));
    assert_eq!(runnable.len(), 2, "skipped installers are not runnable");
    let human = stdout(&fx.run(&["list"]));
    assert!(
        human.contains("[skip: broken upstream]") && human.contains("[overrides: args]"),
        "{human}"
    );
    let rejected = fx.run(&["list", "--tag", "essential"]);
    assert!(!rejected.status.success(), "--tag was removed (ACFS has no tags)");
}

#[test]
fn local_mode_requires_consent_when_not_interactive() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    // No terminal, no --yes, no AFSC_ALLOW_LOCAL: refused with exit 2 before anything runs.
    let out = fx.run_with(&["check", "--local"], &[], &["AFSC_ALLOW_LOCAL"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    assert!(!fx.home.join(".local/share/afsc/results").exists(), "nothing ran");
    // --yes confirms; the warning is still logged.
    let out = fx.run_with(
        &["check", "--local", "--yes", "--format", "jsonl"],
        &[],
        &["AFSC_ALLOW_LOCAL"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("no container isolation"));
    // Dry runs never need consent.
    let out = fx.run_with(&["check", "--local", "--dry-run"], &[], &["AFSC_ALLOW_LOCAL"]);
    assert_eq!(out.status.code(), Some(0));
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

    let human = fx.run(&[
        "classify-error",
        "--stderr",
        "Test timed out after 300s",
        "--exit-code",
        "-1",
        "--explain",
    ]);
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

#[test]
fn url_policy_rejects_http_and_gates_file_urls() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    fx.add_entry("plain_http", "http://example.com/install.sh", &"3".repeat(64));

    // validate: http is a format error (exit 2) regardless of file:// permission.
    let out = fx.run(&["validate", "--format", "json"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let doc = json_doc(&out);
    let errors = doc["format"]["errors"].as_array().unwrap();
    assert!(
        errors
            .iter()
            .any(|e| e.as_str().unwrap().contains("plain_http")
                && e.as_str().unwrap().contains("https")),
        "{errors:?}"
    );

    // check refuses to run a policy-violating installer (exit 2), even when only it is selected.
    let out = fx.run(&["check", "--local", "plain_http"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("URL policy"), "{}", stderr(&out));
    // Selecting only the compliant installer still works.
    let out = fx.run(&["check", "--local", "good_tool", "--format", "jsonl"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(jsonl_lines(&out)[0]["allow_file_urls"], true);

    // file:// without the opt-in is refused; the CLI flag re-enables it.
    let mut strict = Fixture::new();
    strict.add_pass("file_tool");
    strict.set_allow_file_urls(false);
    let out = strict.run(&["check", "--local"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("allow-file-urls"), "{}", stderr(&out));
    let out = strict.run(&["--allow-file-urls", "check", "--local", "--format", "jsonl"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let out = strict.run(&["validate"]);
    assert_eq!(out.status.code(), Some(2));
    let out = strict.run(&["--allow-file-urls", "validate"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn secrets_in_installer_output_are_redacted_everywhere() {
    let mut fx = Fixture::new();
    fx.add_installer(
        "leaky_tool",
        "#!/bin/bash\necho \"GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789\"\necho \"webhook https://hooks.slack.com/services/T0/B0/XYZXYZXYZ\" >&2\nexit 0\n",
    );
    let out = fx.run(&["check", "--local", "--format", "jsonl"]);
    assert_eq!(out.status.code(), Some(0));
    let text = stdout(&out);
    assert!(!text.contains("ghp_abcdefghij"), "token leaked into JSONL: {text}");
    assert!(!text.contains("T0/B0/XYZ"), "webhook leaked into JSONL: {text}");
    let lines = jsonl_lines(&out);
    let r = find_result(&lines, "leaky_tool");
    assert!(r["stdout"].as_str().unwrap().contains("GITHUB_TOKEN=[redacted:"), "{}", r["stdout"]);
    assert!(r["stderr"].as_str().unwrap().contains("[redacted:slack_webhook]"), "{}", r["stderr"]);

    let status = fx.run(&["status", "--format", "json"]);
    let doc = json_doc(&status);
    let entry = &doc["results"][0];
    assert!(!entry["stdout_tail"].as_str().unwrap().contains("ghp_abcdefghij"));
    assert!(!stdout(&status).contains("T0/B0/XYZ"));
}

#[test]
fn install_systemd_dry_run_renders_units_without_touching_the_system() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let out_dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new("bash")
        .arg(root.join("scripts/install-systemd.sh"))
        .args([
            "--dry-run",
            "--user",
            "svc",
            "--data-dir",
            "/srv/afsc",
            "--acfs-repo",
            "/srv/acfs",
            "--out-dir",
        ])
        .arg(out_dir.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    let unit =
        std::fs::read_to_string(out_dir.path().join("automated-flywheel-checker.service")).unwrap();
    assert!(unit.contains("User=svc"), "{unit}");
    assert!(unit.contains("WorkingDirectory=/srv/afsc"), "{unit}");
    assert!(unit.contains("ExecStart=/usr/local/bin/automated_flywheel_setup_checker --config /etc/flywheel-checker/config.toml --format json --watchdog check"), "{unit}");
    assert!(unit.contains("ExecStopPost=/usr/local/bin/automated_flywheel_setup_checker --config /etc/flywheel-checker/config.toml notify --last-run"), "{unit}");
    assert!(!unit.contains('@'), "{unit}");
    assert!(out_dir.path().join("automated-flywheel-checker-serve.service").exists());
    assert!(out_dir.path().join("automated-flywheel-checker.timer").exists());
    assert!(text.contains("[dry-run]"), "{text}");
    assert!(text.contains("acfs_repo=/srv/acfs"), "{text}");
}

#[test]
fn version_reports_git_sha_build_date_and_toolchain() {
    let fx = Fixture::new();
    let out = fx.run(&["--version"]);
    assert_eq!(out.status.code(), Some(0));
    let text = stdout(&out);
    assert!(
        text.starts_with(&format!(
            "automated_flywheel_setup_checker {} (",
            env!("CARGO_PKG_VERSION")
        )),
        "{text}"
    );
    assert!(text.contains(", built 20"), "build date: {text}");
    assert!(text.contains("rustc "), "toolchain: {text}");
    let sha = text.split('(').nth(1).unwrap().split(',').next().unwrap();
    assert!(
        sha == "unknown" || sha.trim_end_matches("-dirty").chars().all(|c| c.is_ascii_hexdigit()),
        "sha: {sha}"
    );
}
