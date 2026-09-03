//! Claude propose/apply modes: an edit session inside a git worktree of the ACFS checkout, gated
//! by a file policy, a transcript safety scan and a re-run of the failing installer before
//! anything is committed. The checkout's current branch and `main` are never touched; a
//! rejected session leaves nothing behind (worktree and branch are discarded).
//!
//! Order of gates, each of which ends the session with `Failed` when it trips:
//!
//! 1. transcript: any High/Critical command in Claude's summary (`is_command_safe`);
//! 2. policy: every changed path must be `checksums.yaml`, the `KNOWN_INSTALLERS` block of
//!    `scripts/lib/security.sh`, or something under `scripts/generated/`;
//! 3. verification: the installer is re-run through the normal executor against the worktree's
//!    `checksums.yaml` (fresh container, or the local sandbox with `--local`); up to
//!    `max_attempts` sessions, each told what still fails.
//!
//! Only then: commit on `afsc/remediate-<installer>-<date>`, push for `apply`, optional PR.

use crate::checksums::parse_checksums;
use crate::config::InstallerOverride;
use crate::remediation::{annotate_risks, ClaudeRemediation, RemediationOutcome, Verification};
use crate::runner::{resolve_spec, GlobalDefaults, InstallerTestRunner, RunnerConfig, TestStatus};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Files Claude may change outright.
pub const POLICY_FILES: &[&str] = &["checksums.yaml", "scripts/lib/security.sh"];
/// Directories under which any change is allowed (generated output).
pub const POLICY_PREFIXES: &[&str] = &["scripts/generated/"];

/// Whether a repo-relative path is inside the edit policy.
pub fn path_allowed(path: &str) -> bool {
    POLICY_FILES.contains(&path) || POLICY_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// 1-based inclusive line range of the `KNOWN_INSTALLERS=(` … `)` block, if present.
pub fn known_installers_block(text: &str) -> Option<(usize, usize)> {
    let mut start = None;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        match start {
            None if line.contains("KNOWN_INSTALLERS=(") => start = Some(n),
            Some(s) if line.trim() == ")" => return Some((s, n)),
            _ => {}
        }
    }
    None
}

/// Every hunk of a `-U0` unified diff must land inside `block` (new-file line numbers). A diff
/// without hunks is not "within" anything.
pub fn hunks_within(diff: &str, block: (usize, usize)) -> bool {
    let mut saw_hunk = false;
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("@@ ") else { continue };
        saw_hunk = true;
        let plus = rest.split_whitespace().find(|t| t.starts_with('+')).unwrap_or("+0");
        let mut parts = plus[1..].split(',');
        let start: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let len: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        let end = if len == 0 { start } else { start + len - 1 };
        if start < block.0 || end > block.1 {
            return false;
        }
    }
    saw_hunk
}

/// A worktree on a fresh branch, created for one edit session.
#[derive(Debug, Clone)]
pub struct EditSession {
    pub branch: String,
    pub worktree: PathBuf,
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `git worktree add -b afsc/remediate-<installer>-<date> <worktrees_dir>/… HEAD`.
pub fn open_worktree(acfs_repo: &Path, worktrees_dir: &Path, installer: &str) -> Result<EditSession> {
    git(acfs_repo, &["rev-parse", "--git-dir"]).context("acfs_repo is not a git repository")?;
    let branch = format!("afsc/remediate-{installer}-{}", Utc::now().format("%Y%m%d-%H%M%S"));
    let worktree = worktrees_dir.join(branch.replace('/', "-"));
    std::fs::create_dir_all(worktrees_dir)?;
    git(acfs_repo, &["worktree", "add", "-b", &branch, &worktree.to_string_lossy(), "HEAD"])?;
    Ok(EditSession { branch, worktree })
}

/// What an edit session changed, and which of it the policy rejects.
#[derive(Debug, Clone, Default)]
pub struct PolicyVerdict {
    pub changed: Vec<String>,
    pub violations: Vec<String>,
    /// `git diff --cached -U0` of the whole worktree
    pub diff: String,
}

/// Stage everything (so new files count) and check each changed path against the policy.
pub fn check_policy(worktree: &Path) -> Result<PolicyVerdict> {
    git(worktree, &["add", "-A"])?;
    let names = git(worktree, &["diff", "--cached", "--name-only"])?;
    let changed: Vec<String> = names.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let diff = git(worktree, &["diff", "--cached", "-U0"])?;
    let mut violations = Vec::new();
    for path in &changed {
        if !path_allowed(path) {
            violations.push(format!("{path}: not in the edit policy"));
            continue;
        }
        if path == "scripts/lib/security.sh" {
            let text = std::fs::read_to_string(worktree.join(path)).unwrap_or_default();
            let file_diff = git(worktree, &["diff", "--cached", "-U0", "--", path])?;
            match known_installers_block(&text) {
                Some(block) if hunks_within(&file_diff, block) => {}
                _ => violations.push(format!("{path}: changes outside the KNOWN_INSTALLERS block")),
            }
        }
    }
    Ok(PolicyVerdict { changed, violations, diff })
}

/// Remove the worktree and delete its branch (best effort; nothing was committed to main).
pub fn discard_worktree(acfs_repo: &Path, session: &EditSession) {
    if let Err(e) = git(acfs_repo, &["worktree", "remove", "--force", &session.worktree.to_string_lossy()]) {
        tracing::warn!(error = %e, "could not remove the worktree");
    }
    if let Err(e) = git(acfs_repo, &["branch", "-D", &session.branch]) {
        tracing::warn!(error = %e, "could not delete the session branch");
    }
}

/// Stage and commit everything in the worktree; returns the commit sha.
pub fn commit_worktree(worktree: &Path, message: &str) -> Result<String> {
    git(worktree, &["add", "-A"])?;
    git(
        worktree,
        &["-c", "user.name=automated_flywheel_setup_checker", "-c", "user.email=afsc@localhost", "commit", "-q", "-m", message],
    )?;
    git(worktree, &["rev-parse", "HEAD"])
}

/// `gh pr create` from the worktree; None when gh is missing or fails (the branch still exists).
pub fn open_pr(worktree: &Path, branch: &str, title: &str, body: &str) -> Option<String> {
    let out = Command::new("gh")
        .current_dir(worktree)
        .args(["pr", "create", "--head", branch, "--title", title, "--body", body])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).lines().last().map(|s| s.trim().to_string()),
        Ok(o) => {
            tracing::warn!(stderr = %String::from_utf8_lossy(&o.stderr).trim(), "gh pr create failed");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "gh not available; branch created without a PR");
            None
        }
    }
}

/// Everything one propose/apply session needs.
pub struct ClaudeEditRequest<'a> {
    pub acfs_repo: &'a Path,
    pub worktrees_dir: &'a Path,
    pub installer: &'a str,
    pub category: &'a str,
    pub stderr: &'a str,
    pub claude: &'a ClaudeRemediation,
    /// `apply` pushes the branch; `propose` only commits
    pub apply: bool,
    pub create_pr: bool,
    pub max_attempts: u32,
    pub allow_bash: bool,
    pub runner_config: RunnerConfig,
    pub installer_override: Option<&'a InstallerOverride>,
    pub globals: GlobalDefaults,
}

async fn verify_in_worktree(req: &ClaudeEditRequest<'_>, worktree: &Path) -> std::result::Result<Verification, String> {
    let path = worktree.join("checksums.yaml");
    let checksums = parse_checksums(&path).map_err(|e| format!("worktree checksums.yaml: {e}"))?;
    let entry = checksums
        .installers
        .get(req.installer)
        .ok_or_else(|| format!("{} is no longer listed in checksums.yaml", req.installer))?;
    let spec = resolve_spec(req.installer, entry, req.installer_override, req.globals);
    let test = spec.to_test();
    let runner = InstallerTestRunner::new(req.runner_config.clone());
    let r = runner.run_test_with_retry(&test).await.map_err(|e| format!("{e:#}"))?;
    Ok(Verification {
        passed: r.status == TestStatus::Passed,
        status: r.status.as_str().to_string(),
        exit_code: r.exit_code,
        duration_ms: r.duration_ms,
        installed_version: r.installed_version.clone(),
        stderr_tail: crate::runner::tail(&r.stderr, 2048),
    })
}

fn first_line(s: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(200).collect()
}

/// Run the gated edit session and turn it into an honest outcome.
pub async fn remediate_with_claude(req: ClaudeEditRequest<'_>) -> RemediationOutcome {
    let session = match open_worktree(req.acfs_repo, req.worktrees_dir, req.installer) {
        Ok(s) => s,
        Err(e) => return RemediationOutcome::Failed { reason: format!("worktree: {e:#}"), cost_usd: 0.0 },
    };
    let fail = |reason: String, cost: f64| {
        discard_worktree(req.acfs_repo, &session);
        RemediationOutcome::Failed { reason, cost_usd: cost }
    };

    let attempts = req.max_attempts.max(1);
    let mut cost = 0.0f64;
    let mut previous: Option<String> = None;
    let mut summary = String::new();
    let mut verified = false;
    for attempt in 1..=attempts {
        let prompt = crate::remediation::generate_edit_prompt(
            req.installer,
            req.category,
            req.stderr,
            &session.worktree,
            previous.as_deref(),
        );
        tracing::info!(installer = req.installer, attempt, branch = %session.branch, "Claude edit session");
        let res = match req.claude.execute_edit_session(&prompt, &session.worktree, req.allow_bash).await {
            Ok(r) => r,
            Err(e) => return fail(format!("claude edit session (attempt {attempt}): {e}"), cost),
        };
        cost += res.envelope.as_ref().map(|e| e.total_cost_usd).unwrap_or(res.estimated_cost_usd as f64);
        summary = crate::reporting::redact(&res.claude_output);

        let risks = annotate_risks(&summary);
        if !risks.is_empty() {
            let list = risks.iter().map(|r| format!("{} [{}]", r.command, r.risk)).collect::<Vec<_>>().join("; ");
            return fail(format!("unsafe command in Claude's transcript: {list}"), cost);
        }
        let verdict = match check_policy(&session.worktree) {
            Ok(v) => v,
            Err(e) => return fail(format!("inspecting the worktree: {e:#}"), cost),
        };
        if !verdict.violations.is_empty() {
            return fail(format!("edit policy violated: {}", verdict.violations.join("; ")), cost);
        }
        if verdict.changed.is_empty() {
            return fail(format!("Claude made no changes: {}", first_line(&summary)), cost);
        }
        match verify_in_worktree(&req, &session.worktree).await {
            Ok(v) if v.passed => {
                verified = true;
                break;
            }
            Ok(v) => {
                tracing::warn!(installer = req.installer, attempt, status = %v.status, "still failing after Claude's edits");
                previous = Some(format!("status {} (exit {:?})\n{}", v.status, v.exit_code, v.stderr_tail));
            }
            Err(e) => previous = Some(e),
        }
    }
    if !verified {
        return fail(
            format!(
                "installer still fails after {attempts} edit session(s): {}",
                first_line(previous.as_deref().unwrap_or("no verification result"))
            ),
            cost,
        );
    }

    let title = format!("fix({}): remediate installer failure ({})", req.installer, req.category);
    let body = format!(
        "{summary}\n\nProposed by automated_flywheel_setup_checker: the installer was re-run against this branch and passed before the commit."
    );
    let sha = match commit_worktree(&session.worktree, &format!("{title}\n\n{body}")) {
        Ok(s) => s,
        Err(e) => return fail(format!("commit: {e:#}"), cost),
    };
    if req.apply {
        if let Err(e) = git(&session.worktree, &["push", "-u", "origin", &session.branch]) {
            return RemediationOutcome::Failed { reason: format!("branch {} committed but push failed: {e:#}", session.branch), cost_usd: cost };
        }
    }
    let pr_url = if req.create_pr { open_pr(&session.worktree, &session.branch, &title, &body) } else { None };
    if req.apply {
        RemediationOutcome::Applied { branch: session.branch.clone(), sha, pr_url, cost_usd: cost }
    } else {
        RemediationOutcome::Proposed { branch: session.branch.clone(), commit: Some(sha), pr_url, cost_usd: cost }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_paths() {
        assert!(path_allowed("checksums.yaml"));
        assert!(path_allowed("scripts/lib/security.sh"));
        assert!(path_allowed("scripts/generated/manifest_index.sh"));
        assert!(!path_allowed("README.md"));
        assert!(!path_allowed("scripts/lib/stack.sh"));
        assert!(!path_allowed("checksums.yaml.bak"));
    }

    #[test]
    fn known_installers_block_and_hunks() {
        let text = "#!/bin/bash\nset -e\ndeclare -gA KNOWN_INSTALLERS=(\n    [uv]=\"https://a\"\n    [bun]=\"https://b\"\n)\necho done\n";
        assert_eq!(known_installers_block(text), Some((3, 6)));
        // A one-line change inside the block (new-file line 5) passes.
        assert!(hunks_within("@@ -5 +5 @@\n-    [bun]=\"https://b\"\n+    [bun]=\"https://c\"\n", (3, 6)));
        // An insertion right before the closing paren passes; an edit to `echo done` (line 7) fails.
        assert!(hunks_within("@@ -5,0 +6,1 @@\n+    [x]=\"https://x\"\n", (3, 7)));
        assert!(!hunks_within("@@ -7 +7 @@\n-echo done\n+echo nope\n", (3, 6)));
        // Two hunks, one outside → rejected; no hunks → rejected.
        assert!(!hunks_within("@@ -4 +4 @@\n-a\n+b\n@@ -1 +1 @@\n-x\n+y\n", (3, 6)));
        assert!(!hunks_within("", (3, 6)));
        assert_eq!(known_installers_block("nothing here"), None);
    }
}
