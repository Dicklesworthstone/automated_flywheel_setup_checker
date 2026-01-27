//! Tests for the notification module
//!
//! Tests cover:
//! - NotificationConfig creation
//! - GitHubConfig and SlackConfig
//! - Notifier creation and configuration access

use automated_flywheel_setup_checker::reporting::{NotificationConfig, Notifier};

// ============================================================================
// NotificationConfig Tests
// ============================================================================

#[test]
fn test_notification_config_no_providers() {
    let config = NotificationConfig { github: None, slack: None };
    assert!(config.github.is_none());
    assert!(config.slack.is_none());
}

#[test]
fn test_notification_config_github_only() {
    let config = NotificationConfig {
        github: Some(automated_flywheel_setup_checker::reporting::GitHubConfig {
            repo: "owner/repo".to_string(),
            token_env: "GITHUB_TOKEN".to_string(),
            create_issues: true,
            add_comments: false,
        }),
        slack: None,
    };
    assert!(config.github.is_some());
    assert!(config.slack.is_none());
    assert_eq!(config.github.as_ref().unwrap().repo, "owner/repo");
}

#[test]
fn test_notification_config_slack_only() {
    let config = NotificationConfig {
        github: None,
        slack: Some(automated_flywheel_setup_checker::reporting::SlackConfig {
            webhook_url_env: "SLACK_WEBHOOK".to_string(),
            channel: "#alerts".to_string(),
            notify_on_failure: true,
            notify_on_success: false,
        }),
    };
    assert!(config.github.is_none());
    assert!(config.slack.is_some());
    assert_eq!(config.slack.as_ref().unwrap().channel, "#alerts");
}

#[test]
fn test_notification_config_both_providers() {
    let config = NotificationConfig {
        github: Some(automated_flywheel_setup_checker::reporting::GitHubConfig {
            repo: "owner/repo".to_string(),
            token_env: "GITHUB_TOKEN".to_string(),
            create_issues: true,
            add_comments: true,
        }),
        slack: Some(automated_flywheel_setup_checker::reporting::SlackConfig {
            webhook_url_env: "SLACK_WEBHOOK".to_string(),
            channel: "#ci".to_string(),
            notify_on_failure: true,
            notify_on_success: true,
        }),
    };
    assert!(config.github.is_some());
    assert!(config.slack.is_some());
}

// ============================================================================
// Notifier Tests
// ============================================================================

#[test]
fn test_notifier_creation() {
    let config = NotificationConfig { github: None, slack: None };
    let notifier = Notifier::new(config);
    assert!(notifier.config().github.is_none());
}

#[test]
fn test_notifier_config_accessor() {
    let config = NotificationConfig {
        github: Some(automated_flywheel_setup_checker::reporting::GitHubConfig {
            repo: "test/repo".to_string(),
            token_env: "TOKEN".to_string(),
            create_issues: false,
            add_comments: false,
        }),
        slack: None,
    };
    let notifier = Notifier::new(config);
    assert_eq!(notifier.config().github.as_ref().unwrap().repo, "test/repo");
}

#[tokio::test]
async fn test_notifier_notify_no_providers() {
    let config = NotificationConfig { github: None, slack: None };
    let notifier = Notifier::new(config);
    // Should succeed even with no providers
    let result = notifier.notify("Test", "Test message", false).await;
    assert!(result.is_ok());
}

// ============================================================================
// Serialization Tests
// ============================================================================

#[test]
fn test_notification_config_serializable() {
    let config = NotificationConfig {
        github: Some(automated_flywheel_setup_checker::reporting::GitHubConfig {
            repo: "owner/repo".to_string(),
            token_env: "GITHUB_TOKEN".to_string(),
            create_issues: true,
            add_comments: false,
        }),
        slack: None,
    };

    let json = serde_json::to_string(&config).unwrap();
    assert!(json.contains("owner/repo"));
    assert!(json.contains("GITHUB_TOKEN"));
}

#[test]
fn test_github_config_clone() {
    let config = automated_flywheel_setup_checker::reporting::GitHubConfig {
        repo: "test/repo".to_string(),
        token_env: "TOKEN".to_string(),
        create_issues: true,
        add_comments: false,
    };
    let cloned = config.clone();
    assert_eq!(config.repo, cloned.repo);
}

#[test]
fn test_slack_config_clone() {
    let config = automated_flywheel_setup_checker::reporting::SlackConfig {
        webhook_url_env: "WEBHOOK".to_string(),
        channel: "#channel".to_string(),
        notify_on_failure: true,
        notify_on_success: false,
    };
    let cloned = config.clone();
    assert_eq!(config.channel, cloned.channel);
}
