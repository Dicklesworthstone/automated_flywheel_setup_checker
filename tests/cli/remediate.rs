//! Remediation: deterministic checksum refresh (advisory diff, verification, propose branch) and
//! read-only Claude advice through the checked-in fake claude (`tests/fixtures/bin/claude`).

use super::support::*;
use std::path::PathBuf;

fn fake_claude_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin")
}

fn fake_claude_bin() -> String {
    fake_claude_dir().join("claude").to_string_lossy().to_string()
}

/// Pin a wrong hash for a passing script and remember the right one.
fn add_drifted(fx: &mut Fixture, name: &str) -> String {
    let path = fx.scripts_dir().join(format!("{name}.sh"));
    std::fs::write(&path, "#!/bin/bash\necho \"installed $0\"\nexit 0\n").unwrap();
    let real = sha256_file(&path);
    fx.add_entry(name, &format!("file://{}", path.display()), &"0".repeat(64));
    real
}

#[test]
fn remediate_checksums_advisory_writes_a_verified_candidate() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    let real = add_drifted(&mut fx, "drifted_tool");

    let out = fx.run(&["remediate", "checksums", "--local", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_no_log_noise(&out);
    let doc = json_doc(&out);
    assert_eq!(doc["kind"], "refresh");
    assert_eq!(doc["mode"], "advisory");
    assert_eq!(doc["plan"]["checked"], 2);
    let entries = doc["plan"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{doc}");
    assert_eq!(entries[0]["name"], "drifted_tool");
    assert_eq!(entries[0]["old_sha256"], "0".repeat(64));
    assert_eq!(entries[0]["new_sha256"], real);
    assert_eq!(entries[0]["verification"]["passed"], true, "{doc}");
    assert!(entries[0]["drift"].is_null(), "no ledger baseline yet");
    let candidate = doc["candidate_path"].as_str().unwrap();
    let text = std::fs::read_to_string(candidate).unwrap();
    assert!(text.contains(&real), "candidate carries the new pin");
    assert!(text.contains("good_tool"), "untouched entries kept");
    assert!(doc["diff"].as_str().unwrap().contains(&format!("+    sha256: \"{real}\"")), "{}", doc["diff"]);
    assert!(doc["proposal"].is_null(), "advisory proposes nothing");
    // The ledger now holds the served script.
    let ledger = fx.home.join(".local/share/afsc/scripts/drifted_tool");
    assert!(ledger.join(format!("{real}.sh")).exists());
    assert!(ledger.join("index.json").exists());

    // Human output shows the plan, the diff and the candidate.
    let human = fx.run(&["remediate", "checksums", "--local"]);
    let text = stdout(&human);
    assert!(text.contains("1 drifted"), "{text}");
    assert!(text.contains("drifted_tool"), "{text}");
    assert!(text.contains("Candidate written to"), "{text}");
    assert!(text.contains("verify: passed"), "{text}");

    // Nothing to do when everything matches.
    let mut clean = Fixture::new();
    clean.add_pass("good_tool");
    let out = clean.run(&["remediate", "checksums", "--local", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json_doc(&out)["plan"]["entries"].as_array().unwrap().len(), 0);
    assert!(stdout(&clean.run(&["remediate", "checksums", "--local"])).contains("No drift"));
}

#[test]
fn remediate_checksums_excludes_entries_that_fail_verification() {
    let mut fx = Fixture::new();
    // Drifted AND broken: the new bytes exit 1, so the refreshed pin must not be proposed.
    let path = fx.scripts_dir().join("broken_tool.sh");
    std::fs::write(&path, "#!/bin/bash\necho 'E: Unable to locate package foo' >&2\nexit 100\n").unwrap();
    fx.add_entry("broken_tool", &format!("file://{}", path.display()), &"0".repeat(64));
    let real = add_drifted(&mut fx, "ok_tool");

    let out = fx.run(&["remediate", "checksums", "--local", "--format", "json"]);
    assert_eq!(out.status.code(), Some(1), "an unverifiable entry is reported as a failure: {}", stderr(&out));
    let doc = json_doc(&out);
    let entries = doc["plan"]["entries"].as_array().unwrap();
    let broken = entries.iter().find(|e| e["name"] == "broken_tool").unwrap();
    assert_eq!(broken["verification"]["passed"], false);
    assert_eq!(broken["verification"]["status"], "failed");
    let candidate = std::fs::read_to_string(doc["candidate_path"].as_str().unwrap()).unwrap();
    assert!(candidate.contains(&real), "verified entry included");
    assert!(candidate.contains(&"0".repeat(64)), "unverified entry keeps its old pin");
    // --no-verify includes everything and exits 0.
    let out = fx.run(&["remediate", "checksums", "--local", "--no-verify", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    let doc = json_doc(&out);
    assert_eq!(doc["verified"], false);
    let candidate = std::fs::read_to_string(doc["candidate_path"].as_str().unwrap()).unwrap();
    assert!(!candidate.contains(&"0".repeat(64)));
}

#[test]
fn remediate_checksums_propose_creates_a_branch_in_a_worktree() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    let real = add_drifted(&mut fx, "drifted_tool");
    // Make the fixture ACFS dir a git repository (the propose path needs one).
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(&fx.acfs).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "."]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);

    let out = fx.run(&["remediate", "checksums", "--local", "--mode", "propose", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let doc = json_doc(&out);
    assert_eq!(doc["mode"], "propose");
    let proposal = &doc["proposal"];
    let branch = proposal["branch"].as_str().unwrap();
    assert!(branch.starts_with("afsc/checksum-refresh-"), "{proposal}");
    assert_eq!(proposal["pushed"], false);
    let worktree = proposal["worktree"].as_str().unwrap();
    assert!(std::fs::read_to_string(std::path::Path::new(worktree).join("checksums.yaml")).unwrap().contains(&real));
    // Original checkout untouched, branch commit message well-formed.
    assert!(std::fs::read_to_string(fx.acfs.join("checksums.yaml")).unwrap().contains(&"0".repeat(64)));
    assert_eq!(git(&["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    let msg = git(&["log", "-1", "--format=%B", branch]);
    assert!(msg.starts_with("chore(checksums): refresh 1 drifted installer pin(s)"), "{msg}");
    assert!(msg.contains("drifted_tool") && msg.contains("verified in a fresh container"), "{msg}");
}

#[test]
fn remediate_from_last_run_uses_the_persisted_mismatches() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    add_drifted(&mut fx, "drifted_tool");
    let nothing = fx.run(&["remediate", "checksums", "--from-last-run", "--local", "--format", "json"]);
    assert_eq!(nothing.status.code(), Some(2), "no runs yet is a usage error");
    let run = fx.run(&["check", "--local", "--format", "jsonl"]);
    assert_eq!(run.status.code(), Some(1));
    let out = fx.run(&["remediate", "checksums", "--from-last-run", "--local", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let doc = json_doc(&out);
    assert_eq!(doc["plan"]["checked"], 1, "only the mismatched installer is fetched");
    assert_eq!(doc["plan"]["entries"][0]["name"], "drifted_tool");
}

#[test]
fn check_remediate_attaches_honest_outcomes() {
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    fx.add_pass("good_tool");
    let real = add_drifted(&mut fx, "drifted_tool");
    fx.add_dependency_failure("dep_tool");
    fx.add_config_toml("[remediation]\nenabled = true\nmode = \"advisory\"\ncost_limit_usd = 5.0\ntimeout_seconds = 20\nmax_attempts = 1\nmax_turns = 3\n");
    let log = fx.root.path().join("claude.log");

    // Without --remediate nothing is attempted and claude is never invoked.
    let out = fx.run_with(
        &["check", "--local", "--format", "jsonl"],
        &[("AFSC_ALLOW_LOCAL", "1"), ("AFSC_CLAUDE_BIN", &fake_claude_bin()), ("AFSC_FAKE_CLAUDE_LOG", log.to_str().unwrap())],
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(jsonl_lines(&out).iter().all(|l| l["remediation"].is_null()));
    assert!(!log.exists(), "claude must not run without --remediate");

    // With --remediate: drift is refreshed and verified; the dependency failure gets advice.
    let out = fx.run_with(
        &["check", "--local", "--remediate", "--format", "jsonl"],
        &[("AFSC_ALLOW_LOCAL", "1"), ("AFSC_CLAUDE_BIN", &fake_claude_bin()), ("AFSC_FAKE_CLAUDE_LOG", log.to_str().unwrap()), ("AFSC_FAKE_CLAUDE", "success")],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "advice does not turn a failure into a pass: {}", stderr(&out));
    let lines = jsonl_lines(&out);
    let drifted = find_result(&lines, "drifted_tool");
    assert_eq!(drifted["remediation"]["outcome"], "verified", "{drifted}");
    assert_eq!(drifted["remediation"]["new_sha256"], real);
    let dep = find_result(&lines, "dep_tool");
    assert_eq!(dep["remediation"]["outcome"], "advised", "{dep}");
    assert_eq!(dep["remediation"]["source"], "claude");
    assert_eq!(dep["remediation"]["cost_usd"], 0.12, "cost from the envelope");
    assert!(dep["remediation"]["suggestion"].as_str().unwrap().contains("apt-get install -y foo"));
    assert_eq!(dep["remediation"]["risks"].as_array().unwrap().len(), 0);
    assert!(find_result(&lines, "good_tool")["remediation"].is_null());
    // Invocation shape: read-only plan mode, never skipping permissions, cwd = ACFS repo.
    // The fake logs argv with printf %q (commas escaped); strip the escapes before matching.
    let invocations = std::fs::read_to_string(&log).unwrap().replace('\\', "");
    assert!(invocations.contains("--permission-mode default"), "{invocations}");
    assert!(invocations.contains("--tools Read,Grep,Glob"), "{invocations}");
    assert!(invocations.contains("--max-turns 3"), "{invocations}");
    assert!(!invocations.contains("dangerously"), "{invocations}");
    assert!(invocations.contains(&format!("cwd={}", fx.acfs.display())), "{invocations}");
    // Persisted and visible in status, counted in metrics.
    let status = json_doc(&fx.run(&["status", "--format", "json"]));
    let persisted = status["results"].as_array().unwrap().iter().find(|r| r["installer_name"] == "dep_tool").unwrap();
    assert_eq!(persisted["remediation"]["outcome"], "advised");
    let prom = stdout(&fx.run(&["status", "--format", "prometheus"]));
    assert!(prom.contains("afsc_remediations_total_24h 2"), "{prom}");
    assert!(prom.contains("afsc_remediations_verified_24h 1"), "{prom}");
    assert!(prom.contains("afsc_remediations_cost_usd_24h 0.12"), "{prom}");
    // Human wording never says "succeeded" for advice.
    let human = fx.run_with(
        &["check", "--local", "--remediate"],
        &[("AFSC_ALLOW_LOCAL", "1"), ("AFSC_CLAUDE_BIN", &fake_claude_bin()), ("AFSC_FAKE_CLAUDE", "success")],
        &[],
    );
    let text = stdout(&human);
    assert!(text.contains("remediation: advice from claude"), "{text}");
    assert!(text.contains("not applied"), "{text}");
    assert!(!text.contains("Remediation succeeded"), "{text}");
}

/// Turn the fixture ACFS dir into a git repo on `main` (the propose path needs one).
fn git_init_fixture(fx: &Fixture) -> impl Fn(&[&str]) -> String + '_ {
    let git = move |args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(&fx.acfs).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "."]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
    git
}

/// PATH with the fixture bin dir first so the fake `gh` answers `pr create`.
fn path_with_fixtures() -> String {
    format!("{}:{}", fake_claude_dir().display(), std::env::var("PATH").unwrap_or_default())
}

/// A checksums.yaml that repoints `dep_tool` at a passing script (what "Claude" will write).
fn fixed_checksums(fx: &Fixture) -> PathBuf {
    let fixed_script = fx.scripts_dir().join("dep_tool_fixed.sh");
    std::fs::write(&fixed_script, "#!/bin/bash\necho fixed\nexit 0\n").unwrap();
    let yaml = format!(
        "# synthetic checksums.yaml (current ACFS format)\ninstallers:\n  dep_tool:\n    url: \"file://{}\"\n    sha256: \"{}\"\n\n",
        fixed_script.display(),
        sha256_file(&fixed_script)
    );
    let path = fx.root.path().join("fixed-checksums.yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

#[test]
fn check_remediate_propose_lands_verified_edits_on_a_branch_with_a_pr() {
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    fx.add_dependency_failure("dep_tool");
    fx.add_config_toml("[remediation]\nenabled = true\nmode = \"propose\"\ncreate_pr = true\ncost_limit_usd = 5.0\ntimeout_seconds = 20\nmax_attempts = 2\nmax_turns = 4\n");
    let git = git_init_fixture(&fx);
    let fixed_yaml = fixed_checksums(&fx);
    let claude_log = fx.root.path().join("claude.log");
    let gh_log = fx.root.path().join("gh.log");
    let path = path_with_fixtures();

    let out = fx.run_with(
        &["check", "--local", "--remediate", "--format", "jsonl"],
        &[
            ("AFSC_ALLOW_LOCAL", "1"),
            ("AFSC_CLAUDE_BIN", &fake_claude_bin()),
            ("AFSC_FAKE_CLAUDE", "edits_in_policy"),
            ("AFSC_FAKE_CLAUDE_EDIT_FROM", fixed_yaml.to_str().unwrap()),
            ("AFSC_FAKE_CLAUDE_LOG", claude_log.to_str().unwrap()),
            ("AFSC_FAKE_GH_LOG", gh_log.to_str().unwrap()),
            ("PATH", &path),
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "a proposal does not turn the run green: {}", stderr(&out));
    let lines = jsonl_lines(&out);
    let dep = find_result(&lines, "dep_tool");
    assert_eq!(dep["remediation"]["outcome"], "proposed", "{dep}");
    let branch = dep["remediation"]["branch"].as_str().unwrap().to_string();
    assert!(branch.starts_with("afsc/remediate-dep_tool-"), "{branch}");
    assert_eq!(dep["remediation"]["pr_url"], "https://github.com/example/acfs/pull/42");
    assert_eq!(dep["remediation"]["cost_usd"], 0.30, "cost from the envelope");
    let commit = dep["remediation"]["commit"].as_str().unwrap().to_string();
    // The branch carries exactly the in-policy edit; main and the checkout are untouched.
    assert_eq!(git(&["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(git(&["rev-parse", &branch]), commit);
    assert_eq!(git(&["diff", "--name-only", "main", &branch]), "checksums.yaml");
    assert!(git(&["show", &format!("{branch}:checksums.yaml")]).contains("dep_tool_fixed.sh"));
    assert!(std::fs::read_to_string(fx.acfs.join("checksums.yaml")).unwrap().contains("dep_tool.sh"));
    let msg = git(&["log", "-1", "--format=%B", &branch]);
    assert!(msg.starts_with("fix(dep_tool): remediate installer failure (dependency)"), "{msg}");
    assert!(msg.contains("re-run against this branch and passed"), "{msg}");
    // Invocation shape: edits accepted only inside the worktree, no Bash, no permission skipping.
    let invocations = std::fs::read_to_string(&claude_log).unwrap().replace('\\', "");
    assert!(invocations.contains("--permission-mode acceptEdits"), "{invocations}");
    assert!(invocations.contains("--tools Read,Grep,Glob,Edit,Write "), "{invocations}");
    assert!(!invocations.contains("Bash"), "{invocations}");
    assert!(invocations.contains("--add-dir "), "{invocations}");
    assert!(!invocations.contains("dangerously"), "{invocations}");
    assert!(invocations.contains("cwd=") && invocations.contains("worktrees"), "{invocations}");
    // PR opened from the worktree on the session branch with a rendered body.
    let gh = std::fs::read_to_string(&gh_log).unwrap().replace('\\', "");
    assert!(gh.contains(&format!("pr create --head {branch}")), "{gh}");
    assert!(gh.contains("--title fix(dep_tool):") && gh.contains("--body"), "{gh}");
    // Human wording says proposed, never succeeded.
    let human = fx.run_with(
        &["check", "--local", "--remediate"],
        &[
            ("AFSC_ALLOW_LOCAL", "1"),
            ("AFSC_CLAUDE_BIN", &fake_claude_bin()),
            ("AFSC_FAKE_CLAUDE", "edits_in_policy"),
            ("AFSC_FAKE_CLAUDE_EDIT_FROM", fixed_yaml.to_str().unwrap()),
            ("PATH", &path),
        ],
        &[],
    );
    let text = stdout(&human);
    assert!(text.contains("proposed on branch afsc/remediate-dep_tool-"), "{text}");
    assert!(!text.contains("succeeded"), "{text}");
}

#[test]
fn check_remediate_propose_rejects_policy_violations_unsafe_transcripts_and_unverified_edits() {
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    fx.add_dependency_failure("dep_tool");
    fx.add_config_toml("[remediation]\nenabled = true\nmode = \"propose\"\ncreate_pr = false\ntimeout_seconds = 20\nmax_attempts = 2\n");
    let git = git_init_fixture(&fx);
    let bin = fake_claude_bin();
    let run = |scenario: &str, extra: &[(&str, &str)]| -> serde_json::Value {
        let mut set: Vec<(&str, &str)> = vec![("AFSC_ALLOW_LOCAL", "1"), ("AFSC_FAKE_CLAUDE", scenario), ("AFSC_CLAUDE_BIN", &bin)];
        set.extend_from_slice(extra);
        let out = fx.run_with(&["check", "--local", "--remediate", "--format", "jsonl"], &set, &[]);
        let lines = jsonl_lines(&out);
        find_result(&lines, "dep_tool")["remediation"].clone()
    };
    let clean = |what: &str| {
        // No worktree and no session branch survive a rejected session.
        let wt = git(&["worktree", "list", "--porcelain"]);
        assert_eq!(wt.matches("worktree ").count(), 1, "{what}: {wt}");
        let branches = git(&["branch", "--list", "afsc/remediate-*"]);
        assert!(branches.is_empty(), "{what}: {branches}");
    };

    // Out-of-policy edit (README.md) → rejected, nothing committed.
    let r = run("edits_out_of_policy", &[]);
    assert_eq!(r["outcome"], "failed", "{r}");
    let reason = r["reason"].as_str().unwrap();
    assert!(reason.contains("policy") && reason.contains("README.md"), "{reason}");
    assert_eq!(r["cost_usd"], 0.30, "cost is still reported");
    clean("out of policy");

    // Unsafe command in the transcript → rejected even though the edit was in policy.
    let r = run("edits_unsafe_transcript", &[]);
    assert_eq!(r["outcome"], "failed", "{r}");
    assert!(r["reason"].as_str().unwrap().contains("unsafe command"), "{r}");
    clean("unsafe transcript");

    // In-policy placeholder edit that removes the installer → verification cannot pass →
    // rejected after max_attempts sessions.
    let log = fx.root.path().join("claude.log");
    let r = run("edits_in_policy", &[("AFSC_FAKE_CLAUDE_LOG", log.to_str().unwrap())]);
    assert_eq!(r["outcome"], "failed", "{r}");
    assert!(r["reason"].as_str().unwrap().contains("still fails after 2 edit session(s)"), "{r}");
    assert_eq!(r["cost_usd"], 0.60, "both sessions' cost is summed");
    let sessions = std::fs::read_to_string(&log).unwrap().matches("argv=").count();
    assert_eq!(sessions, 2, "one session per attempt");
    clean("unverified");
    assert_eq!(git(&["rev-parse", "--abbrev-ref", "HEAD"]), "main");
}

#[test]
fn check_remediate_apply_pushes_the_verified_branch() {
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    fx.add_dependency_failure("dep_tool");
    fx.add_config_toml("[remediation]\nenabled = true\nmode = \"apply\"\ncreate_pr = false\ntimeout_seconds = 20\nmax_attempts = 1\n");
    let git = git_init_fixture(&fx);
    let bare = fx.root.path().join("origin.git");
    let init = std::process::Command::new("git").args(["init", "-q", "--bare", bare.to_str().unwrap()]).output().unwrap();
    assert!(init.status.success());
    git(&["remote", "add", "origin", bare.to_str().unwrap()]);
    let fixed_yaml = fixed_checksums(&fx);
    let env = [
        ("AFSC_ALLOW_LOCAL", "1"),
        ("AFSC_CLAUDE_BIN", &fake_claude_bin()),
        ("AFSC_FAKE_CLAUDE", "edits_in_policy"),
        ("AFSC_FAKE_CLAUDE_EDIT_FROM", fixed_yaml.to_str().unwrap()),
    ];
    let out = fx.run_with(&["check", "--local", "--remediate", "--format", "jsonl"], &env, &[]);
    let lines = jsonl_lines(&out);
    let dep = find_result(&lines, "dep_tool");
    assert_eq!(dep["remediation"]["outcome"], "applied", "{dep}");
    let branch = dep["remediation"]["branch"].as_str().unwrap().to_string();
    let sha = dep["remediation"]["sha"].as_str().unwrap().to_string();
    assert!(dep["remediation"]["pr_url"].is_null());
    // Pushed to origin on the session branch only; origin has no main.
    let remote = std::process::Command::new("git").arg("-C").arg(&bare).args(["branch", "--list"]).output().unwrap();
    let remote_branches = String::from_utf8_lossy(&remote.stdout).to_string();
    assert!(remote_branches.contains(&branch), "{remote_branches}");
    assert!(!remote_branches.contains("main"), "{remote_branches}");
    let remote_sha = std::process::Command::new("git").arg("-C").arg(&bare).args(["rev-parse", &branch]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&remote_sha.stdout).trim(), sha);
    let text = stdout(&fx.run_with(&["check", "--local", "--remediate"], &env, &[]));
    assert!(text.contains("succeeded: pushed afsc/remediate-dep_tool-"), "{text}");
}

#[test]
fn check_remediate_reports_a_budget_cap_once_without_retrying() {
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    fx.add_dependency_failure("dep_tool");
    fx.add_config_toml("[remediation]\nenabled = true\nmode = \"advisory\"\ntimeout_seconds = 20\nmax_attempts = 3\n");
    let log = fx.root.path().join("claude.log");
    let out = fx.run_with(
        &["check", "--local", "--remediate", "--format", "jsonl"],
        &[("AFSC_ALLOW_LOCAL", "1"), ("AFSC_CLAUDE_BIN", &fake_claude_bin()), ("AFSC_FAKE_CLAUDE", "budget"), ("AFSC_FAKE_CLAUDE_LOG", log.to_str().unwrap())],
        &[],
    );
    let lines = jsonl_lines(&out);
    let dep = find_result(&lines, "dep_tool");
    assert_eq!(dep["remediation"]["outcome"], "failed", "{dep}");
    let reason = dep["remediation"]["reason"].as_str().unwrap();
    assert!(reason.contains("Reached maximum budget ($0.05)"), "{reason}");
    assert!(reason.contains("1 turn(s)"), "{reason}");
    assert_eq!(std::fs::read_to_string(&log).unwrap().matches("argv=").count(), 1, "a cap is not retried");
}

#[test]
fn check_remediate_flags_unsafe_advice_and_reports_claude_errors() {
    let mut fx = Fixture::new();
    fx.set_execution(1, 0, false);
    fx.add_dependency_failure("dep_tool");
    fx.add_config_toml("[remediation]\nenabled = true\nmode = \"advisory\"\ntimeout_seconds = 20\nmax_attempts = 1\n");
    let env = |scenario: &str| vec![("AFSC_ALLOW_LOCAL", "1".to_string()), ("AFSC_CLAUDE_BIN", fake_claude_bin()), ("AFSC_FAKE_CLAUDE", scenario.to_string())];

    let e = env("unsafe");
    let set: Vec<(&str, &str)> = e.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let out = fx.run_with(&["check", "--local", "--remediate", "--format", "jsonl"], &set, &[]);
    let lines = jsonl_lines(&out);
    let dep = find_result(&lines, "dep_tool");
    assert_eq!(dep["remediation"]["outcome"], "advised");
    let risks = dep["remediation"]["risks"].as_array().unwrap();
    assert!(risks.iter().any(|r| r["risk"] == "Critical"), "{dep}");
    let human = fx.run_with(&["check", "--local", "--remediate"], &set, &[]);
    assert!(stdout(&human).contains("flagged — NOT applied"), "{}", stdout(&human));
    assert!(stdout(&human).contains("!! Critical"), "{}", stdout(&human));

    let e = env("error");
    let set: Vec<(&str, &str)> = e.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let out = fx.run_with(&["check", "--local", "--remediate", "--format", "jsonl"], &set, &[]);
    let lines = jsonl_lines(&out);
    let dep = find_result(&lines, "dep_tool");
    // is_error envelopes exhaust the attempts and fall back to built-in suggestions.
    assert!(matches!(dep["remediation"]["outcome"].as_str().unwrap(), "advised" | "failed"), "{dep}");
    if dep["remediation"]["outcome"] == "advised" {
        assert_eq!(dep["remediation"]["source"], "fallback", "{dep}");
    }

    let e = env("rate_limit");
    let set: Vec<(&str, &str)> = e.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let out = fx.run_with(&["check", "--local", "--remediate", "--format", "jsonl"], &set, &[]);
    let lines = jsonl_lines(&out);
    let dep = find_result(&lines, "dep_tool");
    assert!(matches!(dep["remediation"]["outcome"].as_str().unwrap(), "advised" | "failed"), "{dep}");
    assert_ne!(dep["remediation"]["source"], "claude");

    // No claude at all: fallback suggestions, still honest.
    let out = fx.run_with(
        &["check", "--local", "--remediate", "--format", "jsonl"],
        &[("AFSC_ALLOW_LOCAL", "1"), ("AFSC_CLAUDE_BIN", "/nonexistent/claude")],
        &[],
    );
    let lines = jsonl_lines(&out);
    let dep = find_result(&lines, "dep_tool");
    assert_eq!(dep["remediation"]["outcome"], "advised", "{dep}");
    assert_eq!(dep["remediation"]["source"], "fallback");
}
