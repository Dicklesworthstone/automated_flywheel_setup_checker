//! Deterministic checksum-drift remediation (no model involved).
//!
//! 1. `plan_refresh`: download every (selected) installer, hash it, and list the entries whose
//!    pinned SHA-256 no longer matches, with a drift report against the last known-good script
//!    from the ledger.
//! 2. `render_candidate`: rewrite `checksums.yaml` textually (comments and order preserved) with
//!    the new hashes — the same shape ACFS's `security.sh --update-checksums` produces.
//! 3. `verify_entry`: run the installer with the new hash through the normal executor (fresh
//!    container or local sandbox); entries that fail verification are excluded from proposals.
//! 4. `propose`: git worktree on `afsc/checksum-refresh-<date>`, commit, optional `gh pr create`;
//!    `apply` additionally pushes the branch (never main).

use crate::checksums::{analyze, ChecksumsFile, DriftReport, Ledger};
use crate::runner::{InstallerSpec, InstallerTestRunner, RunnerConfig, TestStatus};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RefreshEntry {
    pub name: String,
    pub url: String,
    pub old_sha256: String,
    pub new_sha256: String,
    pub size: usize,
    /// Diff/risk against the last known-good script (None when the ledger has no baseline)
    pub drift: Option<DriftReport>,
    /// Set by `verify_entry`
    pub verification: Option<Verification>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Verification {
    pub status: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub installed_version: Option<String>,
    pub stderr_tail: String,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RefreshPlan {
    pub checksums_path: String,
    pub checked: usize,
    pub entries: Vec<RefreshEntry>,
    /// (installer, reason): download failures, missing pins, filtered names
    pub skipped: Vec<(String, String)>,
}

impl RefreshPlan {
    pub fn drifted(&self) -> impl Iterator<Item = &RefreshEntry> {
        self.entries.iter()
    }
    pub fn verified(&self) -> impl Iterator<Item = &RefreshEntry> {
        self.entries.iter().filter(|e| e.verification.as_ref().is_some_and(|v| v.passed))
    }
    pub fn unverified(&self) -> impl Iterator<Item = &RefreshEntry> {
        self.entries.iter().filter(|e| !e.verification.as_ref().is_some_and(|v| v.passed))
    }
}

/// Fetch installer bytes (https via reqwest, file:// directly).
pub async fn fetch_bytes(url: &str) -> Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).with_context(|| format!("reading {path}"));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?;
    let resp = client.get(url).send().await.map_err(|e| e.without_url())?;
    let status = resp.status();
    if !status.is_success() {
        bail!("HTTP {status}");
    }
    Ok(resp.bytes().await.map_err(|e| e.without_url())?.to_vec())
}

/// Build the refresh plan. `only` restricts to installer names; empty = all enabled.
pub async fn plan_refresh(
    checksums_path: &Path,
    checksums: &ChecksumsFile,
    only: &[String],
    ledger: &Ledger,
) -> RefreshPlan {
    let mut plan = RefreshPlan { checksums_path: checksums_path.to_string_lossy().to_string(), ..Default::default() };
    let mut names: Vec<&String> = checksums.installers.keys().collect();
    names.sort();
    for name in names {
        let entry = &checksums.installers[name];
        if !only.is_empty() && !only.iter().any(|n| n == name) {
            continue;
        }
        if !entry.enabled {
            plan.skipped.push((name.clone(), "disabled".into()));
            continue;
        }
        let Some(url) = entry.url.as_deref() else {
            plan.skipped.push((name.clone(), "no url".into()));
            continue;
        };
        let Some(old) = entry.sha256.as_deref().map(|s| s.trim().to_ascii_lowercase()) else {
            plan.skipped.push((name.clone(), "no sha256 pinned".into()));
            continue;
        };
        plan.checked += 1;
        let bytes = match fetch_bytes(url).await {
            Ok(b) => b,
            Err(e) => {
                plan.skipped.push((name.clone(), format!("download failed: {e:#}")));
                continue;
            }
        };
        let new = hex::encode(Sha256::digest(&bytes));
        if new == old {
            // Still matches: remember the bytes as a baseline for future drift diffs.
            let _ = ledger.record(name, &bytes, Some(url), None);
            continue;
        }
        let baseline = ledger.latest_verified(name).or_else(|| ledger.get(name, &old).map(|b| {
            (crate::checksums::LedgerEntry { sha256: old.clone(), size: b.len() as u64, first_seen: Utc::now(), last_seen: Utc::now(), last_verified_pass_run: None, url: None }, b)
        }));
        let drift = baseline.map(|(_, old_bytes)| analyze(&String::from_utf8_lossy(&old_bytes), &String::from_utf8_lossy(&bytes)));
        let _ = ledger.record(name, &bytes, Some(url), None);
        plan.entries.push(RefreshEntry {
            name: name.clone(),
            url: url.to_string(),
            old_sha256: old,
            new_sha256: new,
            size: bytes.len(),
            drift,
            verification: None,
        });
    }
    plan
}

/// Rewrite the `sha256:` line of each drifted installer in the original file text, preserving
/// comments, ordering and quoting. The header comment is refreshed like ACFS's generator does.
pub fn render_candidate(original: &str, plan: &RefreshPlan, verified_only: bool) -> String {
    let updates: BTreeMap<&str, &str> = plan
        .entries
        .iter()
        .filter(|e| !verified_only || e.verification.as_ref().is_some_and(|v| v.passed))
        .map(|e| (e.name.as_str(), e.new_sha256.as_str()))
        .collect();
    let mut out = String::with_capacity(original.len() + 128);
    let mut current: Option<String> = None;
    for (i, line) in original.lines().enumerate() {
        if i == 0 && line.starts_with("# checksums.yaml - Auto-generated") {
            out.push_str(&format!("# checksums.yaml - Auto-generated {}\n", Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00")));
            continue;
        }
        let trimmed = line.trim_end();
        // Installer key lines look like `  name:` (two-space indent, no value).
        if let Some(rest) = line.strip_prefix("  ") {
            if !rest.starts_with(' ') && rest.ends_with(':') && !rest.contains(' ') {
                current = Some(rest.trim_end_matches(':').to_string());
            }
        }
        if let (Some(name), Some(idx)) = (current.as_deref(), trimmed.find("sha256:")) {
            if let Some(new) = updates.get(name) {
                let indent = &line[..line.len() - line.trim_start().len()];
                let quoted = trimmed[idx + "sha256:".len()..].trim().starts_with('"');
                let rendered = if quoted { format!("sha256: \"{new}\"") } else { format!("sha256: {new}") };
                out.push_str(indent);
                out.push_str(&rendered);
                out.push('\n');
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Run one drifted installer with its new hash through the executor; records the verdict.
pub async fn verify_entry(entry: &mut RefreshEntry, spec: &InstallerSpec, runner_config: RunnerConfig) -> bool {
    let mut test = spec.to_test().with_sha256(entry.new_sha256.clone());
    test.name = entry.name.clone();
    let runner = InstallerTestRunner::new(runner_config);
    let verdict = match runner.run_test_with_retry(&test).await {
        Ok(r) => Verification {
            passed: r.status == TestStatus::Passed,
            status: r.status.as_str().to_string(),
            exit_code: r.exit_code,
            duration_ms: r.duration_ms,
            installed_version: r.installed_version.clone(),
            stderr_tail: crate::runner::tail(&r.stderr, 2048),
        },
        Err(e) => Verification {
            passed: false,
            status: "error".into(),
            exit_code: None,
            duration_ms: 0,
            installed_version: None,
            stderr_tail: format!("{e:#}"),
        },
    };
    let passed = verdict.passed;
    entry.verification = Some(verdict);
    passed
}

/// Commit message listing installers and hashes.
pub fn commit_message(plan: &RefreshPlan, verified_only: bool) -> String {
    let entries: Vec<&RefreshEntry> = plan
        .entries
        .iter()
        .filter(|e| !verified_only || e.verification.as_ref().is_some_and(|v| v.passed))
        .collect();
    let mut msg = format!("chore(checksums): refresh {} drifted installer pin(s)\n\n", entries.len());
    for e in &entries {
        msg.push_str(&format!(
            "- {}: {}… → {}… ({}{})\n",
            e.name,
            &e.old_sha256[..12],
            &e.new_sha256[..12],
            e.drift.as_ref().map(|d| d.score.as_str()).unwrap_or("no baseline"),
            e.verification.as_ref().map(|v| if v.passed { ", verified in a fresh container" } else { ", NOT verified" }).unwrap_or("")
        ));
    }
    msg.push_str("\nGenerated by automated_flywheel_setup_checker remediate checksums.\n");
    msg
}

/// Result of a propose/apply step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalResult {
    pub branch: String,
    pub worktree: String,
    pub commit: String,
    pub pr_url: Option<String>,
    pub pushed: bool,
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Create a worktree on a fresh branch, write the candidate, commit; optionally open a PR and push.
/// Never touches the checkout's current branch or `main`.
pub fn propose(
    acfs_repo: &Path,
    worktrees_dir: &Path,
    candidate: &str,
    message: &str,
    create_pr: bool,
    push: bool,
) -> Result<ProposalResult> {
    git(acfs_repo, &["rev-parse", "--git-dir"]).context("acfs_repo is not a git repository")?;
    let date = Utc::now().format("%Y%m%d-%H%M%S");
    let branch = format!("afsc/checksum-refresh-{date}");
    let worktree = worktrees_dir.join(branch.replace('/', "-"));
    std::fs::create_dir_all(worktrees_dir)?;
    git(acfs_repo, &["worktree", "add", "-b", &branch, &worktree.to_string_lossy(), "HEAD"])?;
    let target = worktree.join("checksums.yaml");
    std::fs::write(&target, candidate).with_context(|| format!("writing {}", target.display()))?;
    git(&worktree, &["add", "checksums.yaml"])?;
    git(
        &worktree,
        &["-c", "user.name=automated_flywheel_setup_checker", "-c", "user.email=afsc@localhost", "commit", "-q", "-m", message],
    )?;
    let commit = git(&worktree, &["rev-parse", "HEAD"])?;
    let mut pushed = false;
    let mut pr_url = None;
    if push {
        git(&worktree, &["push", "-u", "origin", &branch])?;
        pushed = true;
    }
    if create_pr {
        let out = Command::new("gh")
            .current_dir(&worktree)
            .args(["pr", "create", "--fill", "--head", &branch])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                pr_url = String::from_utf8_lossy(&o.stdout).lines().last().map(|s| s.trim().to_string());
            }
            Ok(o) => tracing::warn!(stderr = %String::from_utf8_lossy(&o.stderr).trim(), "gh pr create failed"),
            Err(e) => tracing::warn!(error = %e, "gh not available; branch created without a PR"),
        }
    }
    Ok(ProposalResult { branch, worktree: worktree.to_string_lossy().to_string(), commit, pr_url, pushed })
}

/// Where candidates are written for advisory runs.
pub fn candidate_path(data_dir: &Path) -> PathBuf {
    data_dir.join("candidates").join(format!("checksums-{}.yaml", Utc::now().format("%Y%m%dT%H%M%S")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = "# checksums.yaml - Auto-generated 2026-08-26T13:16:05+00:00\n# Run: ./scripts/lib/security.sh --update-checksums\n\ninstallers:\n  atuin:\n    url: \"https://example.com/atuin.sh\"\n    sha256: \"1111111111111111111111111111111111111111111111111111111111111111\"\n\n  uv:\n    url: \"https://example.com/uv.sh\"\n    sha256: \"2222222222222222222222222222222222222222222222222222222222222222\"\n\n  zoxide:\n    url: \"https://example.com/zoxide.sh\"\n    sha256: \"3333333333333333333333333333333333333333333333333333333333333333\"\n";

    fn entry(name: &str, old: char, new: char, passed: Option<bool>) -> RefreshEntry {
        RefreshEntry {
            name: name.into(),
            url: format!("https://example.com/{name}.sh"),
            old_sha256: old.to_string().repeat(64),
            new_sha256: new.to_string().repeat(64),
            size: 10,
            drift: None,
            verification: passed.map(|p| Verification { status: if p { "passed" } else { "failed" }.into(), exit_code: Some(if p { 0 } else { 1 }), duration_ms: 1, installed_version: None, stderr_tail: String::new(), passed: p }),
        }
    }

    #[test]
    fn candidate_rewrites_only_the_drifted_pins_and_keeps_layout() {
        let plan = RefreshPlan { entries: vec![entry("uv", '2', 'a', Some(true)), entry("zoxide", '3', 'b', Some(false))], ..Default::default() };
        let all = render_candidate(ORIGINAL, &plan, false);
        assert!(all.contains("sha256: \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\""));
        assert!(all.contains("sha256: \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\""));
        assert!(all.contains("sha256: \"1111111111111111111111111111111111111111111111111111111111111111\""), "untouched pin kept");
        assert!(all.starts_with("# checksums.yaml - Auto-generated 20"), "header refreshed");
        assert!(all.contains("# Run: ./scripts/lib/security.sh --update-checksums"), "comments kept");
        assert_eq!(all.lines().count(), ORIGINAL.lines().count(), "line structure preserved");

        let verified = render_candidate(ORIGINAL, &plan, true);
        assert!(verified.contains("aaaaaaaaaaaa"));
        assert!(!verified.contains("bbbbbbbbbbbb"), "unverified entry excluded");
        assert!(verified.contains("3333333333333333"));
    }

    #[test]
    fn commit_message_lists_pins_and_verification() {
        let plan = RefreshPlan { entries: vec![entry("uv", '2', 'a', Some(true)), entry("zoxide", '3', 'b', Some(false))], ..Default::default() };
        let msg = commit_message(&plan, true);
        assert!(msg.starts_with("chore(checksums): refresh 1 drifted installer pin(s)"));
        assert!(msg.contains("- uv: 222222222222… → aaaaaaaaaaaa… (no baseline, verified in a fresh container)"), "{msg}");
        assert!(!msg.contains("zoxide"));
        assert_eq!(plan.verified().count(), 1);
        assert_eq!(plan.unverified().count(), 1);
    }

    #[tokio::test]
    async fn plan_detects_drift_against_file_urls_and_records_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let script_ok = dir.path().join("ok.sh");
        let script_drift = dir.path().join("drift.sh");
        std::fs::write(&script_ok, "#!/bin/bash\necho ok\n").unwrap();
        std::fs::write(&script_drift, "#!/bin/bash\nVERSION=2.0.0\necho drift\n").unwrap();
        let ok_sha = hex::encode(Sha256::digest(std::fs::read(&script_ok).unwrap()));
        let yaml = format!(
            "installers:\n  ok_tool:\n    url: \"file://{}\"\n    sha256: \"{ok_sha}\"\n  drift_tool:\n    url: \"file://{}\"\n    sha256: \"{}\"\n  nourl:\n    sha256: \"{}\"\n",
            script_ok.display(),
            script_drift.display(),
            "0".repeat(64),
            "1".repeat(64)
        );
        let path = dir.path().join("checksums.yaml");
        std::fs::write(&path, &yaml).unwrap();
        let checksums = crate::checksums::parse_checksums(&path).unwrap();
        let ledger = Ledger::new(&dir.path().join("data"));
        // Baseline for the drifted tool: an older verified version.
        let old_sha = ledger.record("drift_tool", b"#!/bin/bash\nVERSION=1.0.0\necho drift\n", None, Some("run-0")).unwrap();
        assert_ne!(old_sha, "0".repeat(64));

        let plan = plan_refresh(&path, &checksums, &[], &ledger).await;
        assert_eq!(plan.checked, 2);
        assert_eq!(plan.entries.len(), 1, "{plan:?}");
        let e = &plan.entries[0];
        assert_eq!(e.name, "drift_tool");
        assert_eq!(e.old_sha256, "0".repeat(64));
        let drift = e.drift.as_ref().expect("baseline from the ledger");
        assert_eq!(drift.score, crate::checksums::RiskScore::Routine, "{drift:?}");
        assert!(plan.skipped.iter().any(|(n, r)| n == "nourl" && r == "no url"));
        // Both scripts are now in the ledger (the matching one as a future baseline).
        assert!(ledger.latest("ok_tool").is_some());
        assert!(ledger.get("drift_tool", &e.new_sha256).is_some());

        let only = plan_refresh(&path, &checksums, &["ok_tool".to_string()], &ledger).await;
        assert_eq!(only.checked, 1);
        assert!(only.entries.is_empty());
    }

    #[test]
    fn propose_creates_a_branch_in_a_worktree_with_the_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("acfs");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(repo.join("checksums.yaml"), ORIGINAL).unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]).unwrap();
        let plan = RefreshPlan { entries: vec![entry("uv", '2', 'a', Some(true))], ..Default::default() };
        let candidate = render_candidate(ORIGINAL, &plan, true);
        let result = propose(&repo, &dir.path().join("worktrees"), &candidate, &commit_message(&plan, true), false, false).unwrap();
        assert!(result.branch.starts_with("afsc/checksum-refresh-"));
        assert!(!result.pushed && result.pr_url.is_none());
        let wt = Path::new(&result.worktree);
        assert!(std::fs::read_to_string(wt.join("checksums.yaml")).unwrap().contains("aaaaaaaaaaaa"));
        assert_eq!(git(wt, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(), result.branch);
        // The original checkout is untouched.
        assert!(std::fs::read_to_string(repo.join("checksums.yaml")).unwrap().contains("2222222222222222"));
        assert_eq!(git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(), "main");
        assert!(git(&repo, &["log", "--oneline", "-1", &result.branch]).unwrap().contains("refresh 1 drifted"));
    }
}
