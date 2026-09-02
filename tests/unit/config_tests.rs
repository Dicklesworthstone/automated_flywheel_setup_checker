//! Tests for the configuration module.

use automated_flywheel_setup_checker::config::{
    load_config, Config, DockerConfig, ExecutionConfig, GeneralConfig, MonitoringConfig,
    NotificationsConfig, RemediationConfig, WatchdogConfig,
};
use std::io::Write;
use std::path::PathBuf;
use tempfile::NamedTempFile;

fn write_temp_config(contents: &str) -> NamedTempFile {
    let mut file = NamedTempFile::with_suffix(".toml").unwrap();
    write!(file, "{}", contents).unwrap();
    file
}

#[test]
fn test_load_default_config() {
    let config = load_config(None).unwrap();

    assert_eq!(
        config.general.acfs_repo,
        PathBuf::from("/data/projects/agentic_coding_flywheel_setup")
    );
    assert_eq!(config.general.log_level, "info");
    assert_eq!(config.docker.image, "afsc-base:latest");
    assert_eq!(config.execution.parallel, 1);
    assert!(!config.remediation.enabled);

    assert!(!config.notifications.enabled);
    assert!(config.notifications.slack_webhook_env.is_empty());
    assert!(config.notifications.slack_channel.is_empty());
    assert!(config.notifications.github_token_env.is_empty());
    assert!(config.notifications.github_issue_repo.is_empty());
    assert!(config.notifications.notify_on_failure);
    assert!(!config.notifications.notify_on_success);

    assert!(!config.monitoring.health_endpoint);
    assert_eq!(config.monitoring.health_port, 8080);
    assert!(!config.monitoring.metrics_enabled);
    assert_eq!(config.monitoring.metrics_port, 9090);

    assert_eq!(config.watchdog.default_interval_seconds, 120);
    assert!(!config.watchdog.log_pings);
}

#[test]
fn test_config_default_trait_roundtrip_basics() {
    let config = Config::default();

    assert_eq!(config.docker.image, "afsc-base:latest");
    assert_eq!(config.execution.parallel, 1);
    assert!(!config.notifications.enabled);
    assert_eq!(config.monitoring.health_port, 8080);
    assert_eq!(config.watchdog.default_interval_seconds, 120);
}

#[test]
fn test_individual_section_defaults() {
    let general = GeneralConfig::default();
    let docker = DockerConfig::default();
    let execution = ExecutionConfig::default();
    let remediation = RemediationConfig::default();
    let notifications = NotificationsConfig::default();
    let monitoring = MonitoringConfig::default();
    let watchdog = WatchdogConfig::default();

    assert_eq!(general.log_level, "info");
    assert_eq!(docker.pull_policy, "if-not-present");
    assert_eq!(execution.retry_transient, 3);
    assert_eq!(remediation.max_attempts, 3);
    assert!(notifications.notify_on_failure);
    assert!(!notifications.notify_on_success);
    assert_eq!(monitoring.metrics_port, 9090);
    assert_eq!(watchdog.default_interval_seconds, 120);
}

#[test]
fn test_monitoring_config_defaults() {
    let monitoring = MonitoringConfig::default();

    assert!(!monitoring.health_endpoint);
    assert_eq!(monitoring.health_port, 8080);
    assert!(!monitoring.metrics_enabled);
    assert_eq!(monitoring.metrics_port, 9090);
}

#[test]
fn test_watchdog_config_defaults() {
    let watchdog = WatchdogConfig::default();

    assert_eq!(watchdog.default_interval_seconds, 120);
    assert!(!watchdog.log_pings);
}

#[test]
fn test_full_config_all_seven_sections() {
    let file = write_temp_config(
        r##"
[general]
acfs_repo = "/custom/path"
log_level = "debug"

[docker]
image = "ubuntu:24.04"
memory_limit = "4G"
cpu_quota = 2.0
timeout_seconds = 600
pull_policy = "always"

[execution]
parallel = 4
retry_transient = 5
fail_fast = true

[remediation]
enabled = true
auto_commit = false
create_pr = true
max_attempts = 5

[notifications]
enabled = true
slack_webhook_env = "SLACK_WEBHOOK_URL"
slack_channel = "#ops-alerts"
github_token_env = "GITHUB_TOKEN"
github_issue_repo = "owner/repo"
notify_on_failure = true
notify_on_success = true

[monitoring]
health_endpoint = true
health_port = 8081
metrics_enabled = true
metrics_port = 9191

[watchdog]
default_interval_seconds = 180
log_pings = true
"##,
    );

    let config = load_config(Some(file.path())).unwrap();

    assert_eq!(config.general.acfs_repo, PathBuf::from("/custom/path"));
    assert_eq!(config.general.log_level, "debug");
    assert_eq!(config.docker.image, "ubuntu:24.04");
    assert_eq!(config.docker.memory_limit, "4G");
    assert_eq!(config.docker.cpu_quota, 2.0);
    assert_eq!(config.docker.timeout_seconds, 600);
    assert_eq!(config.execution.parallel, 4);
    assert_eq!(config.execution.retry_transient, 5);
    assert!(config.execution.fail_fast);
    assert!(config.remediation.enabled);
    assert_eq!(config.remediation.max_attempts, 5);

    assert!(config.notifications.enabled);
    assert_eq!(config.notifications.slack_webhook_env, "SLACK_WEBHOOK_URL");
    assert_eq!(config.notifications.slack_channel, "#ops-alerts");
    assert_eq!(config.notifications.github_token_env, "GITHUB_TOKEN");
    assert_eq!(config.notifications.github_issue_repo, "owner/repo");
    assert!(config.notifications.notify_on_failure);
    assert!(config.notifications.notify_on_success);

    assert!(config.monitoring.health_endpoint);
    assert_eq!(config.monitoring.health_port, 8081);
    assert!(config.monitoring.metrics_enabled);
    assert_eq!(config.monitoring.metrics_port, 9191);

    assert_eq!(config.watchdog.default_interval_seconds, 180);
    assert!(config.watchdog.log_pings);
}

#[test]
fn test_config_without_new_sections_uses_defaults() {
    let file = write_temp_config(
        r#"
[general]
acfs_repo = "/my/repo"
log_level = "warn"

[docker]
image = "debian:latest"
memory_limit = "2G"
cpu_quota = 1.0
timeout_seconds = 300
pull_policy = "if-not-present"

[execution]
parallel = 2
retry_transient = 3
fail_fast = false

[remediation]
enabled = false
auto_commit = false
create_pr = true
max_attempts = 3
"#,
    );

    let config = load_config(Some(file.path())).unwrap();

    assert_eq!(config.general.acfs_repo, PathBuf::from("/my/repo"));
    assert_eq!(config.general.log_level, "warn");
    assert_eq!(config.docker.image, "debian:latest");
    assert_eq!(config.execution.parallel, 2);
    assert!(!config.notifications.enabled);
    assert!(config.notifications.slack_webhook_env.is_empty());
    assert!(config.notifications.github_token_env.is_empty());
    assert!(config.notifications.notify_on_failure);
    assert!(!config.notifications.notify_on_success);
    assert_eq!(config.monitoring.health_port, 8080);
    assert_eq!(config.monitoring.metrics_port, 9090);
    assert_eq!(config.watchdog.default_interval_seconds, 120);
    assert!(!config.watchdog.log_pings);
}

#[test]
fn test_load_partial_config_uses_section_defaults() {
    let file = write_temp_config(
        r#"
[notifications]
enabled = true
slack_webhook_env = "SLACK_WEBHOOK_URL"
"#,
    );

    let config = load_config(Some(file.path())).unwrap();

    assert!(config.notifications.enabled);
    assert_eq!(config.notifications.slack_webhook_env, "SLACK_WEBHOOK_URL");
    assert!(config.notifications.slack_channel.is_empty());
    assert!(config.notifications.notify_on_failure);
    assert!(!config.notifications.notify_on_success);
    assert_eq!(config.general.log_level, "info");
    assert_eq!(config.docker.image, "afsc-base:latest");
}

#[test]
fn test_load_nonexistent_file() {
    let result = load_config(Some(std::path::Path::new("/nonexistent/config.toml")));
    assert!(result.is_err());
}

#[test]
fn test_load_invalid_toml() {
    let file = write_temp_config(
        r#"
this is not valid toml
[broken
"#,
    );

    let result = load_config(Some(file.path()));
    assert!(result.is_err());
}

#[test]
fn test_load_wrong_types() {
    let file = write_temp_config(
        r#"
[general]
acfs_repo = 12345
log_level = true
"#,
    );

    let result = load_config(Some(file.path()));
    assert!(result.is_err());
}

#[test]
fn test_empty_file_uses_defaults() {
    let file = NamedTempFile::with_suffix(".toml").unwrap();
    let config = load_config(Some(file.path())).unwrap();

    assert_eq!(config.general.log_level, "info");
    assert_eq!(config.notifications.github_issue_repo, "");
}

#[test]
fn test_notifications_to_internal_enabled_uses_configured_channels() {
    let notifications = NotificationsConfig {
        enabled: true,
        slack_webhook_env: "SLACK_WEBHOOK_URL".to_string(),
        slack_channel: "#alerts".to_string(),
        github_token_env: "GITHUB_TOKEN".to_string(),
        github_issue_repo: "owner/repo".to_string(),
        notify_on_failure: true,
        notify_on_success: false,
        ..Default::default()
    };

    let internal = notifications.to_internal();
    assert!(internal.enabled);

    let github = internal.github.expect("github provider should be configured");
    let slack = internal.slack.expect("slack provider should be configured");

    assert_eq!(github.repo, "owner/repo");
    assert_eq!(github.token_env, "GITHUB_TOKEN");
    assert!(github.create_issues);
    assert!(!github.add_comments);

    assert_eq!(slack.webhook_url_env, "SLACK_WEBHOOK_URL");
    assert_eq!(slack.channel, "#alerts");
    assert!(slack.notify_on_failure);
    assert!(!slack.notify_on_success);
}

#[test]
fn test_notifications_to_internal_disabled_returns_no_providers() {
    let notifications = NotificationsConfig {
        enabled: false,
        slack_webhook_env: "SLACK_WEBHOOK_URL".to_string(),
        slack_channel: "#alerts".to_string(),
        github_token_env: "GITHUB_TOKEN".to_string(),
        github_issue_repo: "owner/repo".to_string(),
        notify_on_failure: true,
        notify_on_success: false,
        ..Default::default()
    };

    let internal = notifications.to_internal();

    assert!(!internal.enabled);
    assert!(internal.github.is_none());
    assert!(internal.slack.is_none());
}

#[test]
fn test_config_serializable() {
    let config = Config::default();
    let serialized = toml::to_string(&config).unwrap();

    assert!(serialized.contains("[docker]"));
    assert!(serialized.contains("[execution]"));
    assert!(serialized.contains("[notifications]"));
    assert!(serialized.contains("[monitoring]"));
    assert!(serialized.contains("[watchdog]"));
}

#[test]
fn test_config_roundtrip() {
    let original = Config::default();
    let serialized = toml::to_string(&original).unwrap();
    let deserialized: Config = toml::from_str(&serialized).unwrap();

    assert_eq!(original.docker.image, deserialized.docker.image);
    assert_eq!(original.execution.parallel, deserialized.execution.parallel);
    assert_eq!(original.remediation.enabled, deserialized.remediation.enabled);
    assert_eq!(
        original.notifications.notify_on_failure,
        deserialized.notifications.notify_on_failure
    );
    assert_eq!(original.monitoring.health_port, deserialized.monitoring.health_port);
    assert_eq!(
        original.watchdog.default_interval_seconds,
        deserialized.watchdog.default_interval_seconds
    );
}
