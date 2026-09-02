//! Structured JSONL event log for runs (`<log_dir>/checker_<YYYYMMDD>.jsonl`).
//!
//! One line per event (`run_started`, `installer_finished`, `run_finished`, `reaper`,
//! `notification`, `remediation`), retained for `log_retention_days` and pruned on open. This is
//! the machine-readable audit trail systemd/logrotate and log shippers consume; it is separate
//! from the human/JSON output on stdout and from the per-run results files.

use super::jsonl::{JsonlReporter, LogEntry, LogLevel, LogRotation};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

pub const LOG_PREFIX: &str = "checker";

/// Append-only event log for the current day.
pub struct EventLog {
    reporter: JsonlReporter,
    path: PathBuf,
    run_id: String,
}

impl EventLog {
    /// Open today's log file under `log_dir`, pruning files older than `retention_days`.
    pub fn open(log_dir: &Path, retention_days: u32, run_id: &str) -> Result<Self> {
        std::fs::create_dir_all(log_dir)
            .with_context(|| format!("Failed to create log directory {}", log_dir.display()))?;
        let rotation = LogRotation::new(log_dir, retention_days.max(1), LOG_PREFIX);
        match rotation.prune_old_logs() {
            Ok(0) => {}
            Ok(n) => debug!(pruned = n, "Pruned old event logs"),
            Err(e) => warn!(error = %e, "Failed to prune old event logs"),
        }
        let path = rotation.current_log_path();
        let reporter = JsonlReporter::new(&path, LogLevel::Debug)?.with_buffer_size(1);
        Ok(Self { reporter, path, run_id: run_id.to_string() })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record an event; failures are logged, never propagated.
    pub fn event(&mut self, event: &str, data: serde_json::Value) {
        self.entry(LogLevel::Info, event, data, None);
    }

    /// Record an installer-scoped event.
    pub fn installer_event(&mut self, event: &str, installer: &str, data: serde_json::Value) {
        self.entry(LogLevel::Info, event, data, Some(installer));
    }

    /// Record a warning-level event.
    pub fn warn_event(&mut self, event: &str, data: serde_json::Value) {
        self.entry(LogLevel::Warn, event, data, None);
    }

    fn entry(&mut self, level: LogLevel, event: &str, data: serde_json::Value, installer: Option<&str>) {
        let mut entry = LogEntry::new(level, "checker", event)
            .with_data(data)
            .with_correlation_id(self.run_id.clone());
        if let Some(name) = installer {
            entry = entry.with_installer(name);
        }
        if let Err(e) = self.reporter.log(entry) {
            warn!(error = %e, path = %self.path.display(), "Failed to write event log");
        }
    }

    pub fn flush(&mut self) {
        let _ = self.reporter.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_events_with_run_id_and_prunes_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("checker_20200101.jsonl");
        std::fs::write(&old, "{}\n").unwrap();
        let mut log = EventLog::open(dir.path(), 7, "run-1").unwrap();
        log.event("run_started", serde_json::json!({"installer_count": 2}));
        log.installer_event("installer_finished", "zoxide", serde_json::json!({"status": "passed"}));
        log.warn_event("reaper", serde_json::json!({"removed": 1}));
        log.flush();
        assert!(!old.exists(), "old log pruned");
        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["event"], "run_started");
        assert_eq!(lines[0]["correlation_id"], "run-1");
        assert_eq!(lines[1]["installer"], "zoxide");
        assert_eq!(lines[2]["level"], "warn");
        // A second open on the same day appends.
        let mut again = EventLog::open(dir.path(), 7, "run-2").unwrap();
        again.event("run_started", serde_json::json!({}));
        again.flush();
        assert_eq!(std::fs::read_to_string(log.path()).unwrap().lines().count(), 4);
    }
}
