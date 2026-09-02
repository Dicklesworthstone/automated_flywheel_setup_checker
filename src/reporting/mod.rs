//! Reporting and notification module

mod jsonl;
mod metrics;
mod notify;
mod redact;
mod summary;

pub use jsonl::{
    AttemptEntry, ErrorClassificationEntry, JsonlReporter, JsonlWriter, LogEntry, LogLevel,
    LogRotation, ResultEntry, ResultPersister, RunFile, RunHeader, RunInfo, RunSummaryEntry,
};
pub use metrics::{MetricsExporter, MetricsSnapshot};
pub use notify::{GitHubConfig, NotificationConfig, Notifier, SlackConfig};
pub use redact::{contains_secret, redact};
pub use summary::{FailureSummary, RunSummary, SummaryGenerator};
