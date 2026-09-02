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
pub use metrics::{MetricsExporter, MetricsSnapshot};
pub use notify::{GitHubConfig, NotificationConfig, Notifier, SlackConfig};
pub use redact::{contains_secret, redact};
pub use summary::{FailureSummary, RunSummary, SummaryGenerator};
