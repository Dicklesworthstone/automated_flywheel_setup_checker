//! Reporting and notification module

mod eventlog;
mod jsonl;
mod metrics;
mod notify;
mod redact;
mod summary;

pub use eventlog::{EventLog, LOG_PREFIX};
pub use jsonl::{
    AttemptEntry, ErrorClassificationEntry, JsonlReporter, JsonlWriter, LogEntry, LogLevel,
    LogRotation, ResultEntry, ResultPersister, RunFile, RunHeader, RunInfo, RunSummaryEntry,
};
pub use metrics::{MetricsExporter, MetricsSnapshot};
pub use notify::{GitHubConfig, NotificationConfig, Notifier, SlackConfig};
pub use redact::{contains_secret, redact};
pub use summary::{FailureSummary, RunSummary, SummaryGenerator};
