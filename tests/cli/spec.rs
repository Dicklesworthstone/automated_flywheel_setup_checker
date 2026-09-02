//! Installer spec resolution through the CLI: overrides, profiles, skips, post-install checks,
//! `--dry-run` rendering, and `validate` cross-checks against a synthetic ACFS checkout.

use super::support::*;

#[test]
fn overrides_apply_interpreter_args_and_env() {
    let mut fx = Fixture::new();
    fx.add_installer(
        "echo_tool",
        "#!/bin/sh\necho \"args=$* MYVAR=$MYVAR shell=$(readlink /proc/$$/exe)\"\nexit 0\n",
    );
    fx.add_override(
        "echo_tool",
        "interpreter = \"sh\"\nargs = [\"--flag\", \"two words\"]\n[installers.echo_tool.env]\nMYVAR = \"seven\"",
    );
    let out = fx.run(&["check", "--local", "--format", "jsonl"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let lines = jsonl_lines(&out);
    let r = find_result(&lines, "echo_tool");
    let stdout_text = r["stdout"].as_str().unwrap();
    assert!(stdout_text.contains("args=--flag two words MYVAR=seven"), "{stdout_text}");
    assert!(!stdout_text.contains("/bash"), "ran under sh: {stdout_text}");
}

#[test]
fn built_in_profile_applies_without_overrides() {
    let mut fx = Fixture::new();
    // The name "ohmyzsh" picks up the ACFS profile: sh with --unattended --keep-zshrc.
    fx.add_installer("ohmyzsh", "#!/bin/sh\necho \"args=$*\"\nexit 0\n");
    let dry = json_doc(&fx.run(&["check", "--local", "--dry-run", "--format", "json"]));
    let spec = &dry["installers"][0];
    assert_eq!(spec["name"], "ohmyzsh");
    assert_eq!(spec["interpreter"], "sh");
    assert_eq!(spec["args"], serde_json::json!(["--unattended", "--keep-zshrc"]));
    assert_eq!(spec["sources"]["interpreter"], "profile");
    assert!(spec["command_line"].as_str().unwrap().ends_with("--unattended --keep-zshrc"));
    let out = fx.run(&["check", "--local", "--format", "jsonl"]);
    let lines = jsonl_lines(&out);
    let r = find_result(&lines, "ohmyzsh");
    assert!(r["stdout"].as_str().unwrap().contains("args=--unattended --keep-zshrc"));
}

#[test]
fn skip_override_is_reported_without_failing_the_run() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    fx.add_dependency_failure("broken_upstream");
    fx.add_override("broken_upstream", "skip = true\nskip_reason = \"upstream installer removed\"");
    let out = fx.run(&["check", "--local", "--format", "jsonl"]);
    assert_eq!(out.status.code(), Some(0), "a skipped installer must not fail the run");
    let lines = jsonl_lines(&out);
    let s = find_result(&lines, "broken_upstream");
    assert_eq!(s["status"], "skipped");
    assert!(s["stderr"].as_str().unwrap().contains("upstream installer removed"));
    let summary = lines.last().unwrap();
    assert_eq!(summary["passed"], 1);
    assert_eq!(summary["failed"], 0);
    assert_eq!(summary["skipped"], 1);
    assert_eq!(summary["exit_code"], 0);

    let human = fx.run(&["check", "--local"]);
    let text = stdout(&human);
    assert!(text.contains("Results: 1 passed, 0 failed, 1 skipped out of 2 total"), "{text}");

    // Persisted and visible in status.
    let status = json_doc(&fx.run(&["status", "--format", "json"]));
    let entry = status["results"].as_array().unwrap().iter().find(|r| r["installer_name"] == "broken_upstream").unwrap();
    assert_eq!(entry["status"], "skipped");
}

#[test]
fn expect_binary_verify_cmd_and_version_cmd_run_after_install() {
    let mut fx = Fixture::new();
    let body = "#!/bin/bash\nmkdir -p \"$HOME/.local/bin\"\nprintf '#!/bin/sh\\necho mytool 1.2.3\\n' > \"$HOME/.local/bin/mytool\"\nchmod +x \"$HOME/.local/bin/mytool\"\nexit 0\n";
    fx.add_installer("tool_ok", body);
    fx.add_installer("tool_missing_bin", body);
    fx.add_installer("tool_verify_fails", body);
    fx.add_override("tool_ok", "expect_binary = \"mytool\"\nversion_cmd = \"mytool --version\"");
    fx.add_override("tool_missing_bin", "expect_binary = \"definitely-not-installed\"");
    fx.add_override("tool_verify_fails", "verify_cmd = \"mytool --version | grep -q 9.9.9\"");

    let out = fx.run(&["check", "--local", "--format", "jsonl"]);
    assert_eq!(out.status.code(), Some(1));
    let lines = jsonl_lines(&out);
    let ok = find_result(&lines, "tool_ok");
    assert_eq!(ok["status"], "passed");
    assert_eq!(ok["installed_version"], "mytool 1.2.3");
    let missing = find_result(&lines, "tool_missing_bin");
    assert_eq!(missing["status"], "failed");
    assert_eq!(missing["error"]["category"], "post_install");
    let vf = find_result(&lines, "tool_verify_fails");
    assert_eq!(vf["status"], "failed");
    assert_eq!(vf["error"]["category"], "post_install");

    let status = json_doc(&fx.run(&["status", "--format", "json"]));
    let entry = status["results"].as_array().unwrap().iter().find(|r| r["installer_name"] == "tool_ok").unwrap();
    assert_eq!(entry["installed_version"], "mytool 1.2.3");
}

#[test]
fn dry_run_shows_the_resolved_spec_with_override_sources() {
    let mut fx = Fixture::new();
    fx.add_pass("plain_tool");
    fx.add_pass("tuned_tool");
    fx.add_override("tuned_tool", "timeout_seconds = 42\nretry = 0\nargs = [\"--quiet\"]\nexpect_binary = \"bash\"");
    let doc = json_doc(&fx.run(&["check", "--local", "--dry-run", "--format", "json"]));
    let installers = doc["installers"].as_array().unwrap();
    let plain = installers.iter().find(|s| s["name"] == "plain_tool").unwrap();
    assert_eq!(plain["timeout_seconds"], 300);
    assert_eq!(plain["retries"], 1, "fixture config sets retry_transient = 1");
    assert_eq!(plain["sources"]["timeout_seconds"], "global");
    assert_eq!(plain["overrides"], serde_json::json!([]));
    let tuned = installers.iter().find(|s| s["name"] == "tuned_tool").unwrap();
    assert_eq!(tuned["timeout_seconds"], 42);
    assert_eq!(tuned["retries"], 0);
    assert_eq!(tuned["sources"]["timeout_seconds"], "override");
    assert!(tuned["command_line"].as_str().unwrap().ends_with("--quiet"));
    let overrides = tuned["overrides"].as_array().unwrap();
    for f in ["args", "expect_binary", "retries", "timeout_seconds"] {
        assert!(overrides.iter().any(|o| o == f), "{f} not in {overrides:?}");
    }

    let human = stdout(&fx.run(&["check", "--local", "--dry-run"]));
    assert!(human.contains("bash /tmp/installer_tuned_tool.sh --quiet"), "{human}");
    assert!(human.contains("[overrides:"), "{human}");
    assert!(human.contains("Would check 2 installer(s)"), "{human}");
}

#[test]
fn validate_cross_checks_known_installers_when_an_acfs_checkout_is_present() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    fx.add_pass("stale_tool");
    let good_url = fx.url_of("good_tool");

    // Missing entry: KNOWN_INSTALLERS has a tool that checksums.yaml lacks → exit 2.
    fx.write_acfs_scripts(&[("good_tool", &good_url), ("brand_new_tool", "https://example.com/new.sh")], &[]);
    let out = fx.run(&["validate", "--format", "json"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let doc = json_doc(&out);
    assert_eq!(doc["format"]["valid"], false);
    assert_eq!(doc["cross_check"]["missing_from_checksums"], serde_json::json!(["brand_new_tool"]));
    assert_eq!(doc["cross_check"]["extra_in_checksums"], serde_json::json!(["stale_tool"]));
    assert!(doc["format"]["errors"][0].as_str().unwrap().contains("brand_new_tool"));

    // URL mismatch is an error too.
    fx.write_acfs_scripts(&[("good_tool", "https://example.com/moved.sh"), ("stale_tool", &fx.url_of("stale_tool"))], &[]);
    let out = fx.run(&["validate", "--format", "json"]);
    assert_eq!(out.status.code(), Some(2));
    let doc = json_doc(&out);
    assert_eq!(doc["cross_check"]["url_mismatches"][0][0], "good_tool");

    // Consistent: clean exit; extras are only warnings.
    fx.write_acfs_scripts(&[("good_tool", &good_url)], &[]);
    let out = fx.run(&["validate", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let doc = json_doc(&out);
    assert_eq!(doc["cross_check"]["extra_in_checksums"], serde_json::json!(["stale_tool"]));
    assert!(doc["format"]["warnings"][0].as_str().unwrap().contains("stale_tool"));
}

#[test]
fn validate_profile_reports_drift_between_acfs_call_sites_and_the_built_in_table() {
    let mut fx = Fixture::new();
    fx.add_pass("zoxide");
    fx.add_pass("rust");
    let modules = "fetch_and_run_with_runner bash $url_q $sha_q zoxide --weird\nfetch_and_run_with_runner sh $url_q $sha_q rust -y\n";
    fx.write_acfs_scripts(
        &[("zoxide", &fx.url_of("zoxide")), ("rust", &fx.url_of("rust"))],
        &[("cli_tools.sh", modules)],
    );
    let out = fx.run(&["validate", "--profile", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "drift is a warning: {}", stderr(&out));
    let doc = json_doc(&out);
    let drift = doc["profile_drift"].as_array().unwrap();
    assert!(drift.iter().any(|d| d["name"] == "zoxide" && d["field"] == "interpreter"), "{drift:?}");
    assert!(drift.iter().any(|d| d["name"] == "zoxide" && d["field"] == "args"), "{drift:?}");
    assert!(!drift.iter().any(|d| d["name"] == "rust"), "rust matches the table: {drift:?}");
    assert!(doc["format"]["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("profile drift for zoxide")));

    // Without a checkout, --profile only warns.
    let mut plain = Fixture::new();
    plain.add_pass("x");
    let out = plain.run(&["validate", "--profile"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("no ACFS checkout"));
}
