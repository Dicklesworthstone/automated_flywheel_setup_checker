//! Tests for notification wiring and config conversion.

use automated_flywheel_setup_checker::config::{Config, NotificationsConfig};
use automated_flywheel_setup_checker::reporting::{
    GitHubConfig, NotificationConfig, Notifier, SlackConfig,
};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

fn env_lock() -> &'static Mutex<()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

fn configured_notifications() -> NotificationsConfig {
    NotificationsConfig {
        enabled: true,
        slack_webhook_env: "SLACK_WEBHOOK_URL".to_string(),
        slack_channel: "#ops-alerts".to_string(),
        github_token_env: "GITHUB_TOKEN".to_string(),
        github_issue_repo: "owner/repo".to_string(),
        notify_on_failure: true,
        notify_on_success: true,
        ..Default::default()
    }
}

#[test]
fn test_notifications_config_defaults() {
    let config = NotificationsConfig::default();

    assert!(!config.enabled);
    assert!(config.slack_webhook_env.is_empty());
    assert!(config.slack_channel.is_empty());
    assert!(config.github_token_env.is_empty());
    assert!(config.github_issue_repo.is_empty());
    assert!(config.notify_on_failure);
    assert!(!config.notify_on_success);
}

#[test]
fn test_notifications_config_from_toml() {
    let config: Config = toml::from_str(
        r#"
[notifications]
enabled = true
slack_webhook_env = "SLACK_WEBHOOK_URL"
slack_channel = "#ops-alerts"
github_token_env = "GITHUB_TOKEN"
github_issue_repo = "owner/repo"
notify_on_failure = true
notify_on_success = true
"#,
    )
    .unwrap();

    assert!(config.notifications.enabled);
    assert_eq!(config.notifications.slack_webhook_env, "SLACK_WEBHOOK_URL");
    assert_eq!(config.notifications.slack_channel, "#ops-alerts");
    assert_eq!(config.notifications.github_token_env, "GITHUB_TOKEN");
    assert_eq!(config.notifications.github_issue_repo, "owner/repo");
    assert!(config.notifications.notify_on_failure);
    assert!(config.notifications.notify_on_success);
}

#[test]
fn test_notifications_config_to_internal() {
    let internal = configured_notifications().to_internal();

    assert!(internal.enabled);

    let github = internal.github.expect("expected github provider");
    assert_eq!(github.repo, "owner/repo");
    assert_eq!(github.token_env, "GITHUB_TOKEN");
    assert!(github.create_issues);
    assert!(!github.add_comments);

    let slack = internal.slack.expect("expected slack provider");
    assert_eq!(slack.webhook_url_env, "SLACK_WEBHOOK_URL");
    assert_eq!(slack.channel, "#ops-alerts");
    assert!(slack.notify_on_failure);
    assert!(slack.notify_on_success);
}

#[tokio::test]
async fn test_notifier_skips_when_disabled() {
    let notifier = Notifier::new(NotificationConfig {
        enabled: false,
        github: Some(GitHubConfig {
            repo: "owner/repo".to_string(),
            token_env: "GITHUB_TOKEN".to_string(),
            create_issues: true,
            add_comments: false,
        }),
        slack: Some(SlackConfig {
            webhook_url_env: "SLACK_WEBHOOK_URL".to_string(),
            channel: "#ops-alerts".to_string(),
            notify_on_failure: true,
            notify_on_success: true,
        }),
    });

    let started = Instant::now();
    let result = notifier.notify("Test", "Body", true).await;

    assert!(result.is_ok());
    assert!(started.elapsed() < Duration::from_millis(1));
}

#[tokio::test]
async fn test_github_skips_missing_token() {
    let _guard = env_lock().lock().unwrap();
    let missing_env = format!("MISSING_GITHUB_TOKEN_{}", uuid::Uuid::new_v4().simple());
    std::env::remove_var(&missing_env);

    let notifier = Notifier::new(NotificationConfig {
        enabled: true,
        github: Some(GitHubConfig {
            repo: "owner/repo".to_string(),
            token_env: missing_env,
            create_issues: true,
            add_comments: false,
        }),
        slack: None,
    });

    let result = notifier.notify("Failure", "Body", true).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_slack_skips_missing_webhook() {
    let _guard = env_lock().lock().unwrap();
    let missing_env = format!("MISSING_SLACK_WEBHOOK_{}", uuid::Uuid::new_v4().simple());
    std::env::remove_var(&missing_env);

    let notifier = Notifier::new(NotificationConfig {
        enabled: true,
        github: None,
        slack: Some(SlackConfig {
            webhook_url_env: missing_env,
            channel: "#ops-alerts".to_string(),
            notify_on_failure: true,
            notify_on_success: false,
        }),
    });

    let result = notifier.notify("Failure", "Body", true).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_github_skips_empty_repo() {
    let notifications = NotificationsConfig {
        github_issue_repo: String::new(),
        github_token_env: "GITHUB_TOKEN".to_string(),
        ..configured_notifications()
    };
    let notifier = Notifier::new(notifications.to_internal());

    let result = notifier.notify("Failure", "Body", true).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_slack_skips_empty_env() {
    let notifications = NotificationsConfig {
        slack_webhook_env: String::new(),
        slack_channel: "#ops-alerts".to_string(),
        ..configured_notifications()
    };
    let notifier = Notifier::new(notifications.to_internal());

    let result = notifier.notify("Failure", "Body", true).await;

    assert!(result.is_ok());
}

#[test]
fn test_notify_on_failure_only() {
    let notifications = NotificationsConfig {
        notify_on_failure: true,
        notify_on_success: false,
        ..configured_notifications()
    };

    let slack = notifications
        .to_internal()
        .slack
        .expect("expected slack provider for failure-only config");

    assert!(slack.notify_on_failure);
    assert!(!slack.notify_on_success);
}

#[test]
fn test_notify_routes_both() {
    let internal = configured_notifications().to_internal();

    assert!(internal.github.is_some());
    assert!(internal.slack.is_some());
}
