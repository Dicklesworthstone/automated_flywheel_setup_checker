//! Reporting and notification module

mod eventlog;
mod history;
mod jsonl;
mod markdown;
mod metrics;
mod notify;
mod redact;
mod summary;

pub use eventlog::{EventLog, LOG_PREFIX};
pub use history::{
    assess_outcomes, diff_runs, is_failure_status, Assessment, DiffEntry, History, HistoryEntry,
    LoadedRun, RunDiff, FAILURE_STATUSES,
};
pub use jsonl::{
    AttemptEntry, ErrorClassificationEntry, JsonlReporter, JsonlWriter, LogEntry, LogLevel,
    LogRotation, ResultEntry, ResultPersister, RunFile, RunHeader, RunInfo, RunSummaryEntry,
};
pub use markdown::{render_diff, render_run, render_timeline};
pub use metrics::{
    HealthState, InstallerMetric, MetricsExporter, MetricsReport, MetricsSnapshot,
    ValidationReport, WINDOW,
};
pub use notify::{
    slack_payload, FailureLine, GitHubConfig, Notification, NotificationConfig, Notifier,
    NotifyOutcome, SlackConfig, DEFAULT_GITHUB_API_URL, DEFAULT_ISSUE_TITLE, ISSUE_LABEL,
};
pub use redact::{contains_secret, redact};
pub use summary::{FailureSummary, RunSummary, SummaryGenerator};
