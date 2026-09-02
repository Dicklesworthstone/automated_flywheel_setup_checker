//! `doctor`: diagnose the environment the checker runs in, with a fix hint per finding.
//!
//! Every check is independent and never panics; the report is one JSON document (`kind: doctor`)
//! or a human table. An abridged form (`DoctorReport::environment()`) is embedded in each run
//! header so results can be reproduced later.

use crate::checksums::{cross_check, is_acfs_repo, parse_checksums, profile_drift, scan_acfs_repo};
use crate::config::{Config, RemediationMode};
use crate::reporting::{History, ValidationReport};
use crate::runner::{ContainerConfig, ContainerManager};
use chrono::Utc;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Status {
    pub fn icon(self) -> &'static str {
        match self {
            Status::Pass => "\u{2713}",
            Status::Warn => "!",
            Status::Fail => "\u{2717}",
            Status::Skip => "-",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Machine-readable facts (versions, paths, counts) for the run header
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>) -> Self {
        Self { name: name.into(), status, detail: detail.into(), hint: None, data: serde_json::Value::Null }
    }
    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    fn data(mut self, data: serde_json::Value) -> Self {
        self.data = data;
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub checked_at: chrono::DateTime<Utc>,
    pub tool_version: String,
    pub checks: Vec<Check>,
    pub passed: usize,
    pub warnings: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl DoctorReport {
    pub fn ok(&self) -> bool {
        self.failed == 0
    }

    /// Abridged environment facts for the run header (versions, image, host).
    pub fn environment(&self) -> serde_json::Value {
        let mut env = serde_json::Map::new();
        for c in &self.checks {
            if !c.data.is_null() {
                env.insert(c.name.clone(), c.data.clone());
            }
        }
        env.insert("doctor_ok".into(), serde_json::json!(self.ok()));
        serde_json::Value::Object(env)
    }

    fn finish(mut self) -> Self {
        self.passed = self.checks.iter().filter(|c| c.status == Status::Pass).count();
        self.warnings = self.checks.iter().filter(|c| c.status == Status::Warn).count();
        self.failed = self.checks.iter().filter(|c| c.status == Status::Fail).count();
        self.skipped = self.checks.iter().filter(|c| c.status == Status::Skip).count();
        self
    }
}

/// Options that change which checks run.
#[derive(Debug, Clone, Default)]
pub struct DoctorOptions {
    /// Skip Docker checks (`--local` deployments)
    pub skip_docker: bool,
    /// Keys the config file contained that the schema does not know
    pub unknown_keys: Vec<String>,
    /// Where the config came from (for the report)
    pub config_path: Option<String>,
}

/// Free bytes on the filesystem holding `path` (via `df`, portable enough on Linux/macOS).
pub fn free_bytes(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("df").arg("-Pk").arg(path).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let avail_kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb * 1024)
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn tool_version(bin: &str) -> Option<String> {
    let path = which::which(bin).ok()?;
    let out = std::process::Command::new(&path).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(if out.stdout.is_empty() { &out.stderr } else { &out.stdout });
    Some(text.lines().next().unwrap_or("").trim().to_string())
}

fn writable(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let probe = dir.join(format!(".afsc-doctor-{}", std::process::id()));
    std::fs::write(&probe, b"ok").map_err(|e| format!("cannot write in {}: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

pub const LOW_DISK_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Run every check.
pub async fn run_doctor(config: &Config, opts: &DoctorOptions) -> DoctorReport {
    let mut checks = Vec::new();

    // Config
    if opts.unknown_keys.is_empty() {
        checks.push(
            Check::new("config", Status::Pass, opts.config_path.clone().unwrap_or_else(|| "built-in defaults".into()))
                .data(serde_json::json!({ "path": opts.config_path })),
        );
    } else {
        checks.push(
            Check::new("config", Status::Warn, format!("unknown keys ignored: {}", opts.unknown_keys.join(", ")))
                .hint("Compare with `config default` and remove or rename the keys"),
        );
    }

    // Docker + image
    let mut docker_root: Option<String> = None;
    if opts.skip_docker {
        checks.push(Check::new("docker", Status::Skip, "skipped (--local)"));
    } else {
        match ContainerManager::try_new(ContainerConfig { image: config.docker.image.clone(), prepare: config.docker.prepare, ..Default::default() }) {
            Ok(manager) => match manager.docker().version().await {
                Ok(v) => {
                    let version = v.version.clone().unwrap_or_default();
                    if let Ok(info) = manager.docker().info().await {
                        docker_root = info.docker_root_dir.clone();
                    }
                    checks.push(
                        Check::new("docker", Status::Pass, format!("daemon {version} ({})", v.api_version.clone().unwrap_or_default()))
                            .data(serde_json::json!({ "version": version, "api": v.api_version, "os": v.os, "arch": v.arch, "kernel": v.kernel_version })),
                    );
                    match manager.image_plan() {
                        Ok(plan) => match manager.docker().inspect_image(&plan.run_image).await {
                            Ok(img) => {
                                let created = img.created.clone().unwrap_or_default();
                                let age = chrono::DateTime::parse_from_rfc3339(&created)
                                    .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_days())
                                    .ok();
                                let status = if age.is_some_and(|d| d > 30) { Status::Warn } else { Status::Pass };
                                let mut c = Check::new(
                                    "image",
                                    status,
                                    format!("{} present{}", plan.run_image, age.map(|d| format!(", built {d} day(s) ago")).unwrap_or_default()),
                                )
                                .data(serde_json::json!({ "tag": plan.run_image, "id": img.id, "created": img.created, "base": plan.base, "prepared": plan.prepared }));
                                if status == Status::Warn {
                                    c = c.hint("Rebuild to pick up base-image security updates: check --rebuild-base");
                                }
                                checks.push(c);
                            }
                            Err(_) => checks.push(
                                Check::new("image", Status::Warn, format!("{} not built yet", plan.run_image))
                                    .hint("The first `check` builds it (a few minutes); or run `check --rebuild-base`")
                                    .data(serde_json::json!({ "tag": plan.run_image, "base": plan.base, "prepared": plan.prepared })),
                            ),
                        },
                        Err(e) => checks.push(Check::new("image", Status::Fail, format!("cannot plan image: {e:#}"))),
                    }
                    let max_age = std::time::Duration::from_secs(config.docker.timeout_seconds.saturating_mul(2).max(60));
                    match (manager.list_managed().await, manager.orphans(max_age).await) {
                        (Ok(all), Ok(leaked)) if leaked.is_empty() => {
                            checks.push(Check::new("containers", Status::Pass, format!("{} afsc-managed container(s), none leaked", all.len())));
                        }
                        (_, Ok(leaked)) => checks.push(
                            Check::new(
                                "containers",
                                Status::Warn,
                                format!(
                                    "{} leaked container(s): {}",
                                    leaked.len(),
                                    leaked.iter().map(|c| format!("{} ({})", c.name, c.reason)).collect::<Vec<_>>().join(", ")
                                ),
                            )
                            .hint("Remove them with: check --reap"),
                        ),
                        (_, Err(e)) => checks.push(Check::new("containers", Status::Warn, format!("cannot list containers: {e:#}"))),
                    }
                }
                Err(e) => checks.push(
                    Check::new("docker", Status::Fail, format!("daemon unreachable: {e:#}"))
                        .hint("Start Docker (systemctl start docker), add the user to the docker group, or use check --local"),
                ),
            },
            Err(e) => checks.push(Check::new("docker", Status::Fail, format!("cannot create client: {e:#}")).hint("Set DOCKER_HOST or start Docker")),
        }
    }

    // ACFS repo + checksums + cross-check
    let repo = &config.general.acfs_repo;
    let checksums_path = repo.join("checksums.yaml");
    if !checksums_path.exists() {
        checks.push(
            Check::new("acfs_repo", Status::Fail, format!("{} has no checksums.yaml", repo.display()))
                .hint("Set [general].acfs_repo (or --acfs-repo) to an agentic_coding_flywheel_setup checkout"),
        );
    } else {
        match parse_checksums(&checksums_path) {
            Ok(checksums) => {
                let enabled = checksums.installers.values().filter(|e| e.enabled).count();
                checks.push(
                    Check::new("acfs_repo", Status::Pass, format!("{} ({} installers, {enabled} enabled)", repo.display(), checksums.installers.len()))
                        .data(serde_json::json!({ "path": repo, "installers": checksums.installers.len(), "enabled": enabled })),
                );
                if is_acfs_repo(repo) {
                    match scan_acfs_repo(repo) {
                        Ok(scan) => {
                            let cc = cross_check(&checksums, &scan.known_installers);
                            let drift = profile_drift(&scan.call_sites);
                            let problems = cc.missing_from_checksums.len() + cc.url_mismatches.len() + drift.len();
                            let detail = format!(
                                "{} KNOWN_INSTALLERS; {} missing from checksums, {} stale, {} URL mismatch(es), {} profile drift(s)",
                                scan.known_installers.len(),
                                cc.missing_from_checksums.len(),
                                cc.extra_in_checksums.len(),
                                cc.url_mismatches.len(),
                                drift.len()
                            );
                            let mut c = Check::new("acfs_cross_check", if problems == 0 { Status::Pass } else { Status::Warn }, detail);
                            if problems > 0 {
                                c = c.hint("Run `validate --profile` for details; update checksums.yaml or the built-in profile table");
                            }
                            checks.push(c);
                        }
                        Err(e) => checks.push(Check::new("acfs_cross_check", Status::Warn, format!("cannot scan repo: {e:#}"))),
                    }
                } else {
                    checks.push(Check::new("acfs_cross_check", Status::Skip, "not a full ACFS checkout (no scripts/lib/security.sh)"));
                }
            }
            Err(e) => checks.push(
                Check::new("acfs_repo", Status::Fail, format!("{} does not parse: {e}", checksums_path.display()))
                    .hint("Run `validate` for the format errors"),
            ),
        }
    }

    // Data / log dirs and disk
    let data_dir = config.general.data_dir_path();
    match writable(&data_dir) {
        Ok(()) => checks.push(Check::new("data_dir", Status::Pass, data_dir.display().to_string()).data(serde_json::json!({ "path": data_dir }))),
        Err(e) => checks.push(Check::new("data_dir", Status::Fail, e).hint("Fix permissions or set [general].data_dir / --data-dir")),
    }
    let log_dir = config.general.log_dir_path();
    match writable(&log_dir) {
        Ok(()) => checks.push(Check::new("log_dir", Status::Pass, log_dir.display().to_string())),
        Err(e) => checks.push(Check::new("log_dir", Status::Fail, e).hint("Fix permissions or set [general].log_dir")),
    }
    let mut disk_targets = vec![("data dir", data_dir.clone())];
    if let Some(root) = docker_root.as_deref() {
        let p = std::path::PathBuf::from(root);
        if p.exists() {
            disk_targets.push(("docker root", p));
        }
    }
    for (label, path) in disk_targets {
        match free_bytes(&path) {
            Some(free) if free < LOW_DISK_BYTES => checks.push(
                Check::new("disk", Status::Warn, format!("{label} {}: only {} free", path.display(), gib(free)))
                    .hint("Free space (docker system prune, results retention) — installers download toolchains"),
            ),
            Some(free) => checks.push(Check::new("disk", Status::Pass, format!("{label} {}: {} free", path.display(), gib(free)))),
            None => checks.push(Check::new("disk", Status::Skip, format!("{label}: cannot determine free space"))),
        }
    }

    // External tools
    let needs_claude = config.remediation.effective_mode() != RemediationMode::Off;
    match (needs_claude, tool_version("claude")) {
        (true, Some(v)) => checks.push(Check::new("claude", Status::Pass, v.clone()).data(serde_json::json!({ "version": v }))),
        (true, None) => checks.push(Check::new("claude", Status::Fail, "claude CLI not found but remediation is enabled").hint("Install Claude Code or set [remediation].mode = \"off\"")),
        (false, Some(v)) => checks.push(Check::new("claude", Status::Pass, format!("{v} (remediation off)"))),
        (false, None) => checks.push(Check::new("claude", Status::Skip, "not installed (remediation off)")),
    }
    let n = &config.notifications;
    let needs_gh = n.enabled && !n.github_issue_repo.trim().is_empty();
    match tool_version("gh") {
        Some(v) => checks.push(Check::new("gh", Status::Pass, v)),
        None if needs_gh => checks.push(Check::new("gh", Status::Skip, "gh CLI not found (notifications use the API directly; gh is only needed for remediation PRs)")),
        None => checks.push(Check::new("gh", Status::Skip, "not installed")),
    }

    // Notification secrets (names only)
    if n.enabled {
        let mut missing = Vec::new();
        for var in [n.slack_webhook_env.trim(), n.github_token_env.trim()] {
            if !var.is_empty() && std::env::var(var).map(|v| v.trim().is_empty()).unwrap_or(true) {
                missing.push(var.to_string());
            }
        }
        if missing.is_empty() {
            checks.push(Check::new("notifications", Status::Pass, format!("enabled (mode {:?})", n.mode)));
        } else {
            checks.push(
                Check::new("notifications", Status::Warn, format!("env var(s) not set: {}", missing.join(", ")))
                    .hint("Export them for the service (systemd: Environment= or an EnvironmentFile)"),
            );
        }
    } else {
        checks.push(Check::new("notifications", Status::Skip, "disabled"));
    }

    // systemd
    let unit = Path::new("/etc/systemd/system/automated-flywheel-checker.service");
    if unit.exists() {
        checks.push(Check::new("systemd", Status::Pass, format!("{} installed", unit.display())));
    } else {
        checks.push(Check::new("systemd", Status::Skip, "units not installed").hint("scripts/install-systemd.sh (see README: Deployment)"));
    }

    // Runs and validation
    match History::load_recent(&config.general.results_dir(), 1) {
        Ok(h) => match h.latest() {
            Some(run) => {
                let age = (Utc::now() - run.started_at()).num_seconds();
                let stale = age > config.monitoring.stale_after_seconds as i64;
                let mut c = Check::new(
                    "last_run",
                    if stale { Status::Warn } else { Status::Pass },
                    format!("{} {} ago ({} passed, {} failed)", run.run_id().chars().take(8).collect::<String>(), humanize(age), run.info.passed, run.info.failed),
                )
                .data(serde_json::json!({ "run_id": run.run_id(), "started_at": run.started_at(), "age_seconds": age }));
                if stale {
                    c = c.hint("Older than monitoring.stale_after_seconds; is the timer enabled?");
                }
                checks.push(c);
            }
            None => checks.push(Check::new("last_run", Status::Warn, "no runs recorded").hint("Run: automated_flywheel_setup_checker check")),
        },
        Err(e) => checks.push(Check::new("last_run", Status::Warn, format!("cannot read results: {e:#}"))),
    }
    match ValidationReport::load(&data_dir) {
        Some(v) => {
            let drift = v.drift_total();
            checks.push(Check::new(
                "validate",
                if drift == 0 { Status::Pass } else { Status::Warn },
                format!("last check-hashes {}: {} matched, {} mismatched, {} unreachable", v.checked_at.map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default(), v.matched, v.mismatched.len(), v.unreachable.len()),
            ));
        }
        None => checks.push(Check::new("validate", Status::Skip, "no hash check recorded").hint("Run: validate --check-hashes")),
    }

    DoctorReport {
        checked_at: Utc::now(),
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        checks,
        passed: 0,
        warnings: 0,
        failed: 0,
        skipped: 0,
    }
    .finish()
}

fn humanize(seconds: i64) -> String {
    if seconds < 90 {
        format!("{seconds}s")
    } else if seconds < 5400 {
        format!("{}m", seconds / 60)
    } else if seconds < 172_800 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

/// Render the report as a human table.
pub fn render_human(report: &DoctorReport) -> String {
    let mut out = String::new();
    for c in &report.checks {
        out.push_str(&format!("{} {:<17} {}\n", c.status.icon(), c.name, c.detail));
        if let Some(h) = &c.hint {
            out.push_str(&format!("    hint: {h}\n"));
        }
    }
    out.push_str(&format!(
        "\n{} passed, {} warning(s), {} failed, {} skipped — {}\n",
        report.passed,
        report.warnings,
        report.failed,
        report.skipped,
        if report.ok() { "ready" } else { "NOT ready" }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_are_sane() {
        assert_eq!(humanize(30), "30s");
        assert_eq!(humanize(600), "10m");
        assert_eq!(humanize(7200), "2h");
        assert_eq!(humanize(300_000), "3d");
        assert!(free_bytes(Path::new("/")).is_some());
        let dir = tempfile::tempdir().unwrap();
        assert!(writable(&dir.path().join("nested")).is_ok());
        assert!(writable(Path::new("/proc/afsc-doctor-nope")).is_err());
    }

    #[tokio::test]
    async fn report_without_docker_or_repo_is_honest() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.general.acfs_repo = dir.path().join("missing");
        config.general.data_dir = dir.path().join("data").to_string_lossy().to_string();
        let opts = DoctorOptions { skip_docker: true, unknown_keys: vec!["docker.imgae".into()], config_path: None };
        let report = run_doctor(&config, &opts).await;
        let by = |n: &str| report.checks.iter().find(|c| c.name == n).unwrap();
        assert_eq!(by("docker").status, Status::Skip);
        assert_eq!(by("acfs_repo").status, Status::Fail);
        assert_eq!(by("config").status, Status::Warn);
        assert_eq!(by("data_dir").status, Status::Pass);
        assert_eq!(by("last_run").status, Status::Warn);
        assert!(!report.ok());
        assert_eq!(report.environment()["doctor_ok"], false);
        assert!(render_human(&report).contains("NOT ready"));
    }
}
