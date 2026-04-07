//! Configuration schema definitions

use crate::reporting::{GitHubConfig, NotificationConfig, SlackConfig};
use serde::{Deserialize, Serialize};
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub docker: DockerConfig,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(default)]
    pub remediation: RemediationConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub monitoring: MonitoringConfig,
    #[serde(default)]
    pub watchdog: WatchdogConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            docker: DockerConfig::default(),
            execution: ExecutionConfig::default(),
            remediation: RemediationConfig::default(),
            notifications: NotificationsConfig::default(),
            monitoring: MonitoringConfig::default(),
            watchdog: WatchdogConfig::default(),
        }
    }
}

/// General configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    /// Path to the ACFS repository
    pub acfs_repo: PathBuf,
    /// Log level (trace, debug, info, warn, error)
    pub log_level: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            acfs_repo: PathBuf::from("/data/projects/agentic_coding_flywheel_setup"),
            log_level: "info".to_string(),
        }
    }
}

/// Docker-related configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerConfig {
    /// Base Docker image to use
    pub image: String,
    /// Memory limit for containers
    pub memory_limit: String,
    /// CPU quota (1.0 = 1 CPU)
    pub cpu_quota: f64,
    /// Timeout in seconds per installer test
    pub timeout_seconds: u64,
    /// Image pull policy: always, if-not-present, never
    pub pull_policy: String,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            image: "afsc-base:latest".to_string(),
            memory_limit: "2G".to_string(),
            cpu_quota: 1.0,
            timeout_seconds: 300,
            pull_policy: "if-not-present".to_string(),
        }
    }
}

/// Execution configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionConfig {
    /// Number of parallel installer tests
    pub parallel: usize,
    /// Number of retries for transient failures
    pub retry_transient: u32,
    /// Stop on first failure
    pub fail_fast: bool,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self { parallel: 1, retry_transient: 3, fail_fast: false }
    }
}

/// Remediation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationConfig {
    /// Enable auto-remediation
    pub enabled: bool,
    /// Auto-commit fixes
    pub auto_commit: bool,
    /// Create PRs for fixes
    pub create_pr: bool,
    /// Maximum remediation attempts
    pub max_attempts: u32,
}

impl Default for RemediationConfig {
    fn default() -> Self {
        Self { enabled: false, auto_commit: false, create_pr: true, max_attempts: 3 }
    }
}

/// Notification configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Enable failure notifications
    pub enabled: bool,
    /// Environment variable holding the Slack webhook URL
    pub slack_webhook_env: String,
    /// Optional Slack channel override
    pub slack_channel: String,
    /// Environment variable holding the GitHub token
    pub github_token_env: String,
    /// GitHub repository for auto-creating failure issues
    pub github_issue_repo: String,
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
            slack_webhook_env: String::new(),
            slack_channel: String::new(),
            github_token_env: String::new(),
            github_issue_repo: String::new(),
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
            add_comments: false,
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
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            health_endpoint: false,
            health_port: default_health_port(),
            metrics_enabled: false,
            metrics_port: default_metrics_port(),
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
