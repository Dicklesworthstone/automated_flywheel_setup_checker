//! Configuration schema definitions
//!
//! Every struct and field has a default so partial sections parse. Resolution order and
//! provenance live in `resolve.rs`; this file is only the shape.

use crate::reporting::{GitHubConfig, NotificationConfig, SlackConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn default_true() -> bool {
    true
}

fn default_health_port() -> u16 {
    8080
}

fn default_metrics_port() -> u16 {
    9090
}

fn default_watchdog_interval() -> u64 {
    120
}

/// Root configuration structure
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub docker: DockerConfig,
    pub execution: ExecutionConfig,
    pub remediation: RemediationConfig,
    pub notifications: NotificationsConfig,
    pub monitoring: MonitoringConfig,
    pub watchdog: WatchdogConfig,
    /// Per-installer overrides, keyed by installer name (`[installers.mdwb]`)
    pub installers: BTreeMap<String, InstallerOverride>,
}

/// Default data directory: `$XDG_DATA_HOME/afsc` or `~/.local/share/afsc`.
pub fn default_data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("afsc");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local").join("share").join("afsc")
}

/// General configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Path to the ACFS repository
    pub acfs_repo: PathBuf,
    /// Log level (trace, debug, info, warn, error)
    pub log_level: String,
    /// Directory for results, metrics, logs, locks, and the script ledger
    /// (empty = `$XDG_DATA_HOME/afsc` or `~/.local/share/afsc`)
    pub data_dir: String,
    /// Number of result files to keep (0 = keep all)
    pub results_retention: u64,
    /// Directory for structured JSONL logs (empty = `<data_dir>/logs`)
    pub log_dir: String,
    /// Days to keep structured logs
    pub log_retention_days: u32,
    /// Allow `file://` installer URLs (tests and local fixtures)
    pub allow_file_urls: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            acfs_repo: PathBuf::from("/data/projects/agentic_coding_flywheel_setup"),
            log_level: "info".to_string(),
            data_dir: String::new(),
            results_retention: 200,
            log_dir: String::new(),
            log_retention_days: 30,
            allow_file_urls: false,
        }
    }
}

impl GeneralConfig {
    /// Effective data directory.
    pub fn data_dir_path(&self) -> PathBuf {
        if self.data_dir.trim().is_empty() {
            default_data_dir()
        } else {
            PathBuf::from(self.data_dir.trim())
        }
    }

    /// Effective log directory.
    pub fn log_dir_path(&self) -> PathBuf {
        if self.log_dir.trim().is_empty() {
            self.data_dir_path().join("logs")
        } else {
            PathBuf::from(&self.log_dir)
        }
    }

    /// Results directory (`<data_dir>/results`).
    pub fn results_dir(&self) -> PathBuf {
        self.data_dir_path().join("results")
    }

    /// Metrics snapshot path (`<data_dir>/metrics.json`).
    pub fn metrics_path(&self) -> PathBuf {
        self.data_dir_path().join("metrics.json")
    }
}

/// Docker-related configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerConfig {
    /// Base Docker image to derive the prepared image from
    pub image: String,
    /// Memory limit for containers
    pub memory_limit: String,
    /// CPU quota (1.0 = 1 CPU)
    pub cpu_quota: f64,
    /// Timeout in seconds per installer test
    pub timeout_seconds: u64,
    /// Image pull policy: always, if-not-present, never
    pub pull_policy: String,
    /// Timeout for building the prepared image
    pub build_timeout_seconds: u64,
    /// Run installers as root inside containers (default: non-root `afsc-user`)
    pub run_as_root: bool,
    /// Remove orphaned afsc-managed containers at startup
    pub reap_orphans: bool,
    /// Container network mode: bridge or none
    pub network: String,
    /// Extra bind mounts for every container, `host:container[:ro]` (local mirrors, fixtures)
    pub volumes: Vec<String>,
    /// Derive a prepared image (ACFS prerequisites + non-root user) from `image`; when false the
    /// image runs as-is (root, no prerequisites)
    pub prepare: bool,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            image: "afsc-base:latest".to_string(),
            memory_limit: "2G".to_string(),
            cpu_quota: 1.0,
            timeout_seconds: 300,
            pull_policy: "if-not-present".to_string(),
            build_timeout_seconds: 900,
            run_as_root: false,
            reap_orphans: true,
            network: "bridge".to_string(),
            volumes: Vec::new(),
            prepare: true,
        }
    }
}

/// Worker count: a fixed number or `"auto"` (= max(1, min(4, cores / 2))).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Parallelism {
    Fixed(usize),
    Auto(String),
}

impl Default for Parallelism {
    fn default() -> Self {
        Parallelism::Fixed(1)
    }
}

impl Parallelism {
    /// Resolve to a worker count using the machine's core count.
    pub fn resolve(&self) -> usize {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
        self.resolve_with_cores(cores)
    }

    pub fn resolve_with_cores(&self, cores: usize) -> usize {
        match self {
            Parallelism::Fixed(n) => (*n).max(1),
            Parallelism::Auto(s) if s.eq_ignore_ascii_case("auto") => (cores / 2).clamp(1, 4),
            Parallelism::Auto(s) => s.trim().parse::<usize>().unwrap_or(1).max(1),
        }
    }
}

impl PartialEq<usize> for Parallelism {
    fn eq(&self, other: &usize) -> bool {
        self.resolve() == *other
    }
}

impl PartialOrd<usize> for Parallelism {
    fn partial_cmp(&self, other: &usize) -> Option<std::cmp::Ordering> {
        Some(self.resolve().cmp(other))
    }
}

impl std::fmt::Display for Parallelism {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Parallelism::Fixed(n) => write!(f, "{n}"),
            Parallelism::Auto(s) => write!(f, "{s}"),
        }
    }
}

/// Installer ordering for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RunOrder {
    /// Historical median duration descending (LPT); falls back to name when no history exists
    #[default]
    LongestFirst,
    /// Alphabetical
    Name,
    /// Order of appearance in checksums.yaml
    Manifest,
}

/// Execution configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutionConfig {
    /// Number of parallel installer tests (integer or "auto")
    pub parallel: Parallelism,
    /// Number of retries for transient failures (attempts = retries + 1)
    pub retry_transient: u32,
    /// Stop on first failure
    pub fail_fast: bool,
    /// Cap on captured stdout/stderr bytes per attempt
    pub max_capture_bytes: u64,
    /// Whole-run deadline in seconds (0 = none)
    pub run_deadline_seconds: u64,
    /// Installer ordering
    pub order: RunOrder,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            parallel: Parallelism::Fixed(1),
            retry_transient: 3,
            fail_fast: false,
            max_capture_bytes: 4 * 1024 * 1024,
            run_deadline_seconds: 0,
            order: RunOrder::LongestFirst,
        }
    }
}

/// Remediation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RemediationMode {
    #[default]
    Off,
    /// Claude runs read-only and prints suggestions; checksum drift produces a reviewable diff
    Advisory,
    /// Changes land on a branch in a git worktree with verification and an optional PR
    Propose,
    /// Propose plus commit and push to the branch
    Apply,
}

impl RemediationMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "advisory" => Some(Self::Advisory),
            "propose" => Some(Self::Propose),
            "apply" => Some(Self::Apply),
            _ => None,
        }
    }
}

/// Remediation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RemediationConfig {
    /// Enable auto-remediation (equivalent to `mode != off`)
    pub enabled: bool,
    /// Remediation mode: off, advisory, propose, apply
    pub mode: RemediationMode,
    /// Auto-commit fixes (apply mode)
    pub auto_commit: bool,
    /// Create PRs for fixes (propose/apply modes)
    pub create_pr: bool,
    /// Maximum remediation attempts per failure
    pub max_attempts: u32,
    /// Per-run Claude spend cap in USD
    pub cost_limit_usd: f64,
    /// Maximum agent turns per Claude invocation
    pub max_turns: u32,
    /// Timeout per Claude invocation in seconds
    pub timeout_seconds: u64,
    /// Let propose/apply edit sessions run shell commands (needed for `bun run generate`);
    /// off by default so Claude can only read and edit files inside the worktree
    pub allow_bash: bool,
}

impl Default for RemediationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: RemediationMode::Off,
            auto_commit: false,
            create_pr: true,
            max_attempts: 3,
            // One `claude --print` invocation costs ~$0.13 before it reads anything (measured
            // 2026-09-02 with Claude Code 2.1.259); 1.0 left no room for a real advisory run.
            cost_limit_usd: 3.0,
            max_turns: 12,
            timeout_seconds: 300,
            allow_bash: false,
        }
    }
}

impl RemediationConfig {
    /// Effective mode: `enabled = true` with `mode = off` means advisory (backwards compatible).
    pub fn effective_mode(&self) -> RemediationMode {
        match (self.enabled, self.mode) {
            (true, RemediationMode::Off) => RemediationMode::Advisory,
            (_, mode) => mode,
        }
    }
}

/// Notification mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotificationMode {
    EveryRun,
    /// Only when the set of failing installers or drifted checksums changed (default)
    #[default]
    OnChange,
    DailyDigest,
}

/// Notification configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Enable failure notifications
    pub enabled: bool,
    /// When to send: every_run, on_change, daily_digest
    pub mode: NotificationMode,
    /// Environment variable holding the Slack webhook URL
    pub slack_webhook_env: String,
    /// Optional Slack channel override
    pub slack_channel: String,
    /// Environment variable holding the GitHub token
    pub github_token_env: String,
    /// GitHub repository for auto-creating failure issues
    pub github_issue_repo: String,
    /// Comment on the open rolling issue instead of opening a new one per run
    #[serde(default = "default_true")]
    pub github_add_comments: bool,
    /// GitHub API base URL (tests point this at a mock server)
    pub github_api_url: String,
    /// Title of the rolling issue; also the dedup key
    pub github_issue_title: String,
    /// Notify Slack for failures
    #[serde(default = "default_true")]
    pub notify_on_failure: bool,
    /// Notify Slack for successful runs
    pub notify_on_success: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: NotificationMode::OnChange,
            slack_webhook_env: String::new(),
            slack_channel: String::new(),
            github_token_env: String::new(),
            github_issue_repo: String::new(),
            github_add_comments: true,
            github_api_url: crate::reporting::DEFAULT_GITHUB_API_URL.to_string(),
            github_issue_title: crate::reporting::DEFAULT_ISSUE_TITLE.to_string(),
            notify_on_failure: default_true(),
            notify_on_success: false,
        }
    }
}

impl NotificationsConfig {
    /// Convert the user-facing config shape into the internal notifier configuration.
    pub fn to_internal(&self) -> NotificationConfig {
        if !self.enabled {
            return NotificationConfig { enabled: false, github: None, slack: None };
        }

        let github = (!self.github_issue_repo.trim().is_empty()
            || !self.github_token_env.trim().is_empty())
        .then(|| GitHubConfig {
            repo: self.github_issue_repo.trim().to_string(),
            token_env: self.github_token_env.trim().to_string(),
            create_issues: true,
            add_comments: self.github_add_comments,
            api_url: if self.github_api_url.trim().is_empty() {
                crate::reporting::DEFAULT_GITHUB_API_URL.to_string()
            } else {
                self.github_api_url.trim().to_string()
            },
            issue_title: if self.github_issue_title.trim().is_empty() {
                crate::reporting::DEFAULT_ISSUE_TITLE.to_string()
            } else {
                self.github_issue_title.trim().to_string()
            },
        });

        let slack = (!self.slack_webhook_env.trim().is_empty()
            || !self.slack_channel.trim().is_empty())
        .then(|| SlackConfig {
            webhook_url_env: self.slack_webhook_env.trim().to_string(),
            channel: self.slack_channel.trim().to_string(),
            notify_on_failure: self.notify_on_failure,
            notify_on_success: self.notify_on_success,
        });

        NotificationConfig { enabled: true, github, slack }
    }
}

/// Monitoring configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitoringConfig {
    /// Enable the health check endpoint
    pub health_endpoint: bool,
    /// Port used for the health check endpoint
    #[serde(default = "default_health_port")]
    pub health_port: u16,
    /// Enable metrics collection
    pub metrics_enabled: bool,
    /// Port used for metrics scraping
    #[serde(default = "default_metrics_port")]
    pub metrics_port: u16,
    /// Bind address for the monitoring listener
    pub bind: String,
    /// `/health` reports `stale` when the last run is older than this many seconds
    pub stale_after_seconds: u64,
    /// HTTP status returned for a stale health check
    pub stale_status_code: u16,
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            health_endpoint: false,
            health_port: default_health_port(),
            metrics_enabled: false,
            metrics_port: default_metrics_port(),
            bind: "0.0.0.0".to_string(),
            stale_after_seconds: 93_600,
            stale_status_code: 503,
        }
    }
}

/// Watchdog configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchdogConfig {
    /// Fallback watchdog ping interval in seconds
    #[serde(default = "default_watchdog_interval")]
    pub default_interval_seconds: u64,
    /// Log watchdog pings at debug level
    pub log_pings: bool,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self { default_interval_seconds: default_watchdog_interval(), log_pings: false }
    }
}

/// Per-installer override (`[installers.<name>]`). Every field is optional; unset fields fall
/// back to the built-in ACFS profile and then to the global settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct InstallerOverride {
    pub timeout_seconds: Option<u64>,
    /// Retries for this installer (attempts = retry + 1)
    pub retry: Option<u32>,
    /// `bash` or `sh`
    pub interpreter: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub skip: Option<bool>,
    pub skip_reason: Option<String>,
    /// Binary that must be on PATH after a passing install
    pub expect_binary: Option<String>,
    /// Command run after a passing install; non-zero → category `post_install`
    pub verify_cmd: Option<String>,
    /// Command whose first output line is stored as `installed_version`
    pub version_cmd: Option<String>,
    pub run_as_root: Option<bool>,
    pub memory_limit: Option<String>,
    /// `bridge` or `none`
    pub network: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_sections_parse() {
        let c: Config = toml::from_str("[docker]\ntimeout_seconds = 7\n").unwrap();
        assert_eq!(c.docker.timeout_seconds, 7);
        assert_eq!(c.docker.image, "afsc-base:latest");
        let c: Config = toml::from_str("[execution]\nparallel = \"auto\"\n").unwrap();
        assert!(c.execution.parallel.resolve_with_cores(8) >= 1);
        assert_eq!(c.execution.parallel.resolve_with_cores(8), 4);
        assert_eq!(c.execution.parallel.resolve_with_cores(2), 1);
    }

    #[test]
    fn installer_override_parses() {
        let c: Config = toml::from_str(
            "[installers.ohmyzsh]\ninterpreter = \"sh\"\nargs = [\"--unattended\", \"--keep-zshrc\"]\nskip = false\n\n[installers.atuin.env]\nATUIN_NO_MODIFY_PATH = \"1\"\n",
        )
        .unwrap();
        assert_eq!(c.installers["ohmyzsh"].interpreter.as_deref(), Some("sh"));
        assert_eq!(c.installers["ohmyzsh"].args.as_ref().unwrap().len(), 2);
        assert_eq!(c.installers["atuin"].env["ATUIN_NO_MODIFY_PATH"], "1");
    }

    #[test]
    fn remediation_effective_mode_is_backwards_compatible() {
        let c = RemediationConfig { enabled: true, ..Default::default() };
        assert_eq!(c.effective_mode(), RemediationMode::Advisory);
        let c = RemediationConfig {
            enabled: false,
            mode: RemediationMode::Propose,
            ..Default::default()
        };
        assert_eq!(c.effective_mode(), RemediationMode::Propose);
        assert_eq!(RemediationMode::parse("APPLY"), Some(RemediationMode::Apply));
        assert_eq!(RemediationMode::parse("nope"), None);
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.docker.image, "afsc-base:latest");
        assert_eq!(back.execution.order, RunOrder::LongestFirst);
        assert_eq!(back.notifications.mode, NotificationMode::OnChange);
    }
}
