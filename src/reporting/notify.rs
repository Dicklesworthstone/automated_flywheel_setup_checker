//! Notification handlers (GitHub issues with dedup, Slack blocks).
//!
//! GitHub lifecycle: one rolling issue per repo (matched by label + title prefix). A failing run
//! comments on the open issue (or creates it); a clean run comments "recovered" and closes it.
//! Secrets never reach logs: only environment variable *names* are logged and reqwest errors are
//! logged without their URL.

use anyhow::{Context, Result};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";
pub const DEFAULT_ISSUE_TITLE: &str = "AFSC canary: installer failures";
pub const ISSUE_LABEL: &str = "afsc-automated";

/// Notification configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    pub enabled: bool,
    pub github: Option<GitHubConfig>,
    pub slack: Option<SlackConfig>,
}

/// GitHub notification config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubConfig {
    pub repo: String,
    pub token_env: String,
    pub create_issues: bool,
    /// Comment on the existing open issue instead of opening a new one
    pub add_comments: bool,
    /// API base (overridable for tests)
    #[serde(default = "default_api_url")]
    pub api_url: String,
    /// Title of the rolling issue (also the dedup key)
    #[serde(default = "default_issue_title")]
    pub issue_title: String,
}

fn default_api_url() -> String {
    DEFAULT_GITHUB_API_URL.to_string()
}

fn default_issue_title() -> String {
    DEFAULT_ISSUE_TITLE.to_string()
}

/// Slack notification config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackConfig {
    pub webhook_url_env: String,
    pub channel: String,
    pub notify_on_failure: bool,
    pub notify_on_success: bool,
}

/// One failing installer, as shown in notifications.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FailureLine {
    pub installer: String,
    pub status: String,
    pub category: String,
    pub severity: String,
    pub duration_ms: u64,
    pub attempts: usize,
    /// First meaningful stderr line (already redacted)
    pub hint: String,
}

/// What gets sent: a title, a Markdown body (GitHub), structured fields (Slack).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub title: String,
    pub body_markdown: String,
    pub is_failure: bool,
    pub run_id: String,
    pub summary_fields: Vec<(String, String)>,
    pub failures: Vec<FailureLine>,
    /// `failure`, `recovered`, `success`, `digest`
    pub kind: String,
}

/// What happened on each channel (for the event log and `notify` output).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NotifyOutcome {
    /// created | commented | closed | skipped | unconfigured | failed
    pub github: Option<String>,
    pub github_issue: Option<u64>,
    /// sent | skipped | unconfigured | failed
    pub slack: Option<String>,
}

/// Notification sender
pub struct Notifier {
    config: NotificationConfig,
    client: reqwest::Client,
}

impl Notifier {
    pub fn new(config: NotificationConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self { config, client }
    }

    /// Send a notification (both channels, each independently tolerant of failure).
    pub async fn send(&self, n: &Notification) -> Result<NotifyOutcome> {
        let mut outcome = NotifyOutcome::default();
        if !self.config.enabled {
            return Ok(outcome);
        }

        if let Some(github) = &self.config.github {
            match self.github(github, n).await {
                Ok((action, issue)) => {
                    outcome.github = Some(action);
                    outcome.github_issue = issue;
                }
                Err(e) => {
                    warn!(repo = %github.repo, error = %e, "GitHub notification failed");
                    outcome.github = Some("failed".into());
                }
            }
        }

        if let Some(slack) = &self.config.slack {
            let wanted = (n.is_failure && slack.notify_on_failure)
                || (!n.is_failure && slack.notify_on_success);
            if !wanted {
                outcome.slack = Some("skipped".into());
            } else {
                match self.slack(slack, n).await {
                    Ok(sent) => outcome.slack = Some(sent.to_string()),
                    Err(e) => {
                        warn!(error = %e, "Slack notification failed");
                        outcome.slack = Some("failed".into());
                    }
                }
            }
        }

        Ok(outcome)
    }

    /// Backwards-compatible plain-text entry point.
    pub async fn notify(&self, title: &str, message: &str, is_failure: bool) -> Result<()> {
        let n = Notification {
            title: title.to_string(),
            body_markdown: message.to_string(),
            is_failure,
            run_id: String::new(),
            summary_fields: Vec::new(),
            failures: Vec::new(),
            kind: if is_failure { "failure".into() } else { "success".into() },
        };
        self.send(&n).await.map(|_| ())
    }

    fn github_token(github: &GitHubConfig) -> Option<String> {
        if github.repo.trim().is_empty() {
            info!("Skipping GitHub notification: repo not configured");
            return None;
        }
        if github.token_env.trim().is_empty() {
            info!("Skipping GitHub notification: token env var name not configured");
            return None;
        }
        match std::env::var(&github.token_env) {
            Ok(value) if !value.trim().is_empty() => Some(value),
            _ => {
                info!(env_var = %github.token_env, "Skipping GitHub notification: token env var not set");
                None
            }
        }
    }

    async fn github(
        &self,
        github: &GitHubConfig,
        n: &Notification,
    ) -> Result<(String, Option<u64>)> {
        let Some(token) = Self::github_token(github) else {
            return Ok(("unconfigured".into(), None));
        };
        if !github.repo.contains('/') {
            warn!(repo = %github.repo, "Skipping GitHub notification: expected owner/repo");
            return Ok(("unconfigured".into(), None));
        }
        let api = github.api_url.trim_end_matches('/');
        let base = format!("{api}/repos/{}/issues", github.repo.trim());

        let existing = self.find_open_issue(&base, &token, &github.issue_title).await?;

        if n.is_failure {
            match existing {
                Some(number) if github.add_comments => {
                    self.post_comment(&base, &token, number, &n.body_markdown).await?;
                    info!(repo = %github.repo, issue = number, "Commented on the open AFSC issue");
                    Ok(("commented".into(), Some(number)))
                }
                Some(number) => {
                    info!(repo = %github.repo, issue = number, "Open AFSC issue exists; add_comments is off");
                    Ok(("skipped".into(), Some(number)))
                }
                None if github.create_issues => {
                    let number = self
                        .create_issue(&base, &token, &github.issue_title, &n.body_markdown)
                        .await?;
                    info!(repo = %github.repo, issue = number, "Created GitHub issue");
                    Ok(("created".into(), Some(number)))
                }
                None => Ok(("skipped".into(), None)),
            }
        } else {
            match existing {
                Some(number) => {
                    let comment = format!("Recovered: {}\n\n{}", n.title, n.body_markdown);
                    self.post_comment(&base, &token, number, &comment).await?;
                    self.close_issue(&base, &token, number).await?;
                    info!(repo = %github.repo, issue = number, "Closed the AFSC issue after a clean run");
                    Ok(("closed".into(), Some(number)))
                }
                None => Ok(("skipped".into(), None)),
            }
        }
    }

    fn github_request(
        &self,
        method: reqwest::Method,
        url: &str,
        token: &str,
    ) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(ACCEPT, "application/vnd.github+json")
            .header(USER_AGENT, concat!("afsc/", env!("CARGO_PKG_VERSION")))
    }

    async fn find_open_issue(&self, base: &str, token: &str, title: &str) -> Result<Option<u64>> {
        let url = format!("{base}?state=open&labels={ISSUE_LABEL}&per_page=50");
        let response = self
            .github_request(reqwest::Method::GET, &url, token)
            .send()
            .await
            .map_err(|e| e.without_url())
            .context("listing open issues")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("listing open issues returned {status}: {}", excerpt(&body));
        }
        let issues: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap_or_default();
        Ok(issues
            .iter()
            .filter(|i| i.get("pull_request").is_none())
            .find(|i| i["title"].as_str().is_some_and(|t| t.starts_with(title)))
            .and_then(|i| i["number"].as_u64()))
    }

    async fn create_issue(&self, base: &str, token: &str, title: &str, body: &str) -> Result<u64> {
        let response = self
            .github_request(reqwest::Method::POST, base, token)
            .json(&json!({ "title": title, "body": body, "labels": [ISSUE_LABEL] }))
            .send()
            .await
            .map_err(|e| e.without_url())
            .context("creating issue")?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status != reqwest::StatusCode::CREATED {
            anyhow::bail!("creating issue returned {status}: {}", excerpt(&text));
        }
        let value: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        Ok(value["number"].as_u64().unwrap_or(0))
    }

    async fn post_comment(&self, base: &str, token: &str, number: u64, body: &str) -> Result<()> {
        let url = format!("{base}/{number}/comments");
        let response = self
            .github_request(reqwest::Method::POST, &url, token)
            .json(&json!({ "body": body }))
            .send()
            .await
            .map_err(|e| e.without_url())
            .context("commenting on issue")?;
        let status = response.status();
        if status != reqwest::StatusCode::CREATED {
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("commenting on issue #{number} returned {status}: {}", excerpt(&text));
        }
        Ok(())
    }

    async fn close_issue(&self, base: &str, token: &str, number: u64) -> Result<()> {
        let url = format!("{base}/{number}");
        let response = self
            .github_request(reqwest::Method::PATCH, &url, token)
            .json(&json!({ "state": "closed", "state_reason": "completed" }))
            .send()
            .await
            .map_err(|e| e.without_url())
            .context("closing issue")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("closing issue #{number} returned {status}: {}", excerpt(&text));
        }
        Ok(())
    }

    async fn slack(&self, slack: &SlackConfig, n: &Notification) -> Result<&'static str> {
        if slack.webhook_url_env.trim().is_empty() {
            info!("Skipping Slack notification: webhook env var name not configured");
            return Ok("unconfigured");
        }
        let webhook_url = match std::env::var(&slack.webhook_url_env) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => {
                info!(env_var = %slack.webhook_url_env, "Skipping Slack notification: webhook env var not set");
                return Ok("unconfigured");
            }
        };

        let mut payload = slack_payload(n);
        if !slack.channel.trim().is_empty() {
            payload["channel"] = json!(slack.channel.trim());
        }

        let response = self
            .client
            .post(&webhook_url)
            .header(CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| e.without_url())
            .context("posting to the Slack webhook")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Slack webhook returned {status}: {}", excerpt(&body));
        }
        info!("Sent Slack notification");
        Ok("sent")
    }

    pub fn config(&self) -> &NotificationConfig {
        &self.config
    }
}

/// Slack Block Kit payload: header, summary fields, one section per failure (max 10), run id.
pub fn slack_payload(n: &Notification) -> serde_json::Value {
    let mut blocks = vec![json!({
        "type": "header",
        "text": { "type": "plain_text", "text": n.title.chars().take(150).collect::<String>() }
    })];
    if !n.summary_fields.is_empty() {
        let fields: Vec<serde_json::Value> = n
            .summary_fields
            .iter()
            .take(10)
            .map(|(k, v)| json!({ "type": "mrkdwn", "text": format!("*{k}*\n{v}") }))
            .collect();
        blocks.push(json!({ "type": "section", "fields": fields }));
    }
    for f in n.failures.iter().take(10) {
        let mut text = format!(
            "*{}* — {} ({}, {}) — {:.1}s, {} attempt(s)",
            f.installer,
            f.status,
            f.category,
            f.severity,
            f.duration_ms as f64 / 1000.0,
            f.attempts
        );
        if !f.hint.is_empty() {
            text.push_str(&format!("\n`{}`", f.hint.chars().take(200).collect::<String>()));
        }
        blocks.push(json!({ "type": "section", "text": { "type": "mrkdwn", "text": text } }));
    }
    if n.failures.len() > 10 {
        blocks.push(json!({
            "type": "context",
            "elements": [{ "type": "mrkdwn", "text": format!("… and {} more", n.failures.len() - 10) }]
        }));
    }
    if !n.run_id.is_empty() {
        blocks.push(json!({
            "type": "context",
            "elements": [{ "type": "mrkdwn", "text": format!("run `{}` · kind {}", n.run_id, n.kind) }]
        }));
    }
    let color = if n.is_failure { "#d73a49" } else { "#28a745" };
    json!({
        "text": n.title,
        "blocks": blocks,
        "attachments": [{ "color": color, "text": n.body_markdown.chars().take(2800).collect::<String>() }],
    })
}

fn excerpt(input: &str) -> String {
    const MAX_LEN: usize = 200;

    if input.chars().count() <= MAX_LEN {
        input.to_string()
    } else {
        let mut shortened: String = input.chars().take(MAX_LEN).collect();
        shortened.push_str("...");
        shortened
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_payload_has_header_fields_and_failure_sections() {
        let n = Notification {
            title: "AFSC: 2 failures in 5 tests".into(),
            body_markdown: "## body".into(),
            is_failure: true,
            run_id: "run-1".into(),
            summary_fields: vec![("Passed".into(), "3".into()), ("Failed".into(), "2".into())],
            failures: vec![
                FailureLine {
                    installer: "rust".into(),
                    status: "failed".into(),
                    category: "network".into(),
                    severity: "Transient".into(),
                    duration_ms: 1500,
                    attempts: 4,
                    hint: "curl: (7) Failed".into(),
                },
                FailureLine {
                    installer: "bun".into(),
                    status: "timedout".into(),
                    category: "timeout".into(),
                    severity: "Transient".into(),
                    duration_ms: 300000,
                    attempts: 1,
                    hint: String::new(),
                },
            ],
            kind: "failure".into(),
        };
        let p = slack_payload(&n);
        let blocks = p["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(blocks[1]["fields"].as_array().unwrap().len(), 2);
        assert!(blocks[2]["text"]["text"]
            .as_str()
            .unwrap()
            .contains("*rust* — failed (network, Transient)"));
        assert!(blocks[2]["text"]["text"].as_str().unwrap().contains("curl: (7) Failed"));
        assert!(blocks[3]["text"]["text"].as_str().unwrap().contains("300.0s"));
        assert!(blocks.last().unwrap()["elements"][0]["text"]
            .as_str()
            .unwrap()
            .contains("run `run-1`"));
        assert_eq!(p["attachments"][0]["color"], "#d73a49");
    }
}
