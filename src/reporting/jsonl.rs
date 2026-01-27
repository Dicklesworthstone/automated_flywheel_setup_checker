//! Structured JSONL logging

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

/// Log level for structured entries
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Default for LogLevel {
    fn default() -> Self {
        LogLevel::Info
    }
}

/// Structured log entry for JSONL output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,
    pub component: String,
    pub event: String,
    pub data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installer: Option<String>,
}

impl LogEntry {
    /// Create a new log entry with the given level and event
    pub fn new(level: LogLevel, component: impl Into<String>, event: impl Into<String>) -> Self {
        Self {
            timestamp: Utc::now(),
            level,
            component: component.into(),
            event: event.into(),
            data: serde_json::Value::Null,
            duration_ms: None,
            error: None,
            correlation_id: None,
            installer: None,
        }
    }

    pub fn info(component: impl Into<String>, event: impl Into<String>) -> Self {
        Self::new(LogLevel::Info, component, event)
    }

    pub fn error(component: impl Into<String>, event: impl Into<String>) -> Self {
        Self::new(LogLevel::Error, component, event)
    }

    pub fn warn(component: impl Into<String>, event: impl Into<String>) -> Self {
        Self::new(LogLevel::Warn, component, event)
    }

    pub fn debug(component: impl Into<String>, event: impl Into<String>) -> Self {
        Self::new(LogLevel::Debug, component, event)
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = data;
        self
    }

    pub fn with_duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }

    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    pub fn with_installer(mut self, installer: impl Into<String>) -> Self {
        self.installer = Some(installer.into());
        self
    }
}

/// Simple writer for JSONL (JSON Lines) format logs
pub struct JsonlWriter {
    writer: BufWriter<File>,
}

impl JsonlWriter {
    /// Create a new JSONL writer
    pub fn new(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;

        Ok(Self { writer: BufWriter::new(file) })
    }

    /// Write a record to the JSONL file
    pub fn write<T: Serialize>(&mut self, record: &T) -> Result<()> {
        let json = serde_json::to_string(record)?;
        writeln!(self.writer, "{}", json)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Flush the writer
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}

/// Reporter for structured JSONL logs with batching and level filtering
pub struct JsonlReporter {
    writer: BufWriter<File>,
    min_level: LogLevel,
    buffer_size: usize,
    pending_entries: Vec<LogEntry>,
    fsync_enabled: bool,
}

impl JsonlReporter {
    /// Create a new JSONL reporter
    pub fn new(path: &Path, min_level: LogLevel) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;

        Ok(Self {
            writer: BufWriter::new(file),
            min_level,
            buffer_size: 100,
            pending_entries: Vec::new(),
            fsync_enabled: false,
        })
    }

    /// Enable fsync after each write (for durability)
    pub fn with_fsync(mut self, enabled: bool) -> Self {
        self.fsync_enabled = enabled;
        self
    }

    /// Set buffer size for batch writes
    pub fn with_buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size;
        self
    }

    /// Log an entry if it meets the minimum level
    pub fn log(&mut self, entry: LogEntry) -> Result<()> {
        if entry.level >= self.min_level {
            self.pending_entries.push(entry);

            if self.pending_entries.len() >= self.buffer_size {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// Log an entry only if the condition is true
    pub fn log_if(&mut self, condition: bool, entry: LogEntry) -> Result<()> {
        if condition {
            self.log(entry)?;
        }
        Ok(())
    }

    /// Log a batch of entries
    pub fn log_batch(&mut self, entries: Vec<LogEntry>) -> Result<()> {
        for entry in entries {
            self.log(entry)?;
        }
        Ok(())
    }

    /// Flush pending entries to disk
    pub fn flush(&mut self) -> Result<()> {
        for entry in self.pending_entries.drain(..) {
            let json = serde_json::to_string(&entry)?;
            writeln!(self.writer, "{}", json)?;
        }
        self.writer.flush()?;

        if self.fsync_enabled {
            self.writer.get_ref().sync_all()?;
        }

        Ok(())
    }

    /// Get the current minimum log level
    pub fn min_level(&self) -> LogLevel {
        self.min_level
    }
}

impl Drop for JsonlReporter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Manager for log rotation and pruning
pub struct LogRotation {
    log_dir: std::path::PathBuf,
    retention_days: u32,
    file_prefix: String,
}

impl LogRotation {
    /// Create a new log rotation manager
    pub fn new(
        log_dir: impl Into<std::path::PathBuf>,
        retention_days: u32,
        file_prefix: impl Into<String>,
    ) -> Self {
        Self {
            log_dir: log_dir.into(),
            retention_days,
            file_prefix: file_prefix.into(),
        }
    }

    /// Get the path for today's log file
    pub fn current_log_path(&self) -> std::path::PathBuf {
        let date = Utc::now().format("%Y%m%d");
        self.log_dir.join(format!("{}_{}.jsonl", self.file_prefix, date))
    }

    /// Prune log files older than retention period
    ///
    /// Returns the number of files deleted
    pub fn prune_old_logs(&self) -> Result<usize> {
        use std::fs;

        let cutoff = Utc::now() - chrono::Duration::days(self.retention_days as i64);
        let cutoff_str = cutoff.format("%Y%m%d").to_string();

        let mut deleted_count = 0;

        if !self.log_dir.exists() {
            return Ok(0);
        }

        for entry in fs::read_dir(&self.log_dir)? {
            let entry = entry?;
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                // Match files like "checker_20260126.jsonl"
                if name.starts_with(&self.file_prefix) && name.ends_with(".jsonl") {
                    // Extract date from filename
                    if let Some(date_str) = name
                        .strip_prefix(&format!("{}_", self.file_prefix))
                        .and_then(|s| s.strip_suffix(".jsonl"))
                    {
                        if date_str < cutoff_str.as_str() {
                            tracing::info!(path = %path.display(), "Pruning old log file");
                            fs::remove_file(&path)?;
                            deleted_count += 1;
                        }
                    }
                }
            }
        }

        Ok(deleted_count)
    }

    /// Get all log files sorted by date (newest first)
    pub fn list_log_files(&self) -> Result<Vec<std::path::PathBuf>> {
        use std::fs;

        let mut files = Vec::new();

        if !self.log_dir.exists() {
            return Ok(files);
        }

        for entry in fs::read_dir(&self.log_dir)? {
            let entry = entry?;
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with(&self.file_prefix) && name.ends_with(".jsonl") {
                    files.push(path);
                }
            }
        }

        // Sort by filename (date) in reverse order
        files.sort_by(|a, b| b.cmp(a));

        Ok(files)
    }

    /// Get the retention period in days
    pub fn retention_days(&self) -> u32 {
        self.retention_days
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use tempfile::NamedTempFile;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct TestRecord {
        name: String,
        value: i32,
    }

    #[test]
    fn test_jsonl_writer() {
        let file = NamedTempFile::new().unwrap();
        let mut writer = JsonlWriter::new(file.path()).unwrap();

        let record = TestRecord { name: "test".to_string(), value: 42 };
        writer.write(&record).unwrap();

        let content = std::fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"name\":\"test\""));
        assert!(content.contains("\"value\":42"));
    }

    #[test]
    fn test_log_entry_builder() {
        let entry = LogEntry::info("runner", "test_started")
            .with_installer("nodejs")
            .with_correlation_id("run-123")
            .with_data(serde_json::json!({"version": "20.0"}));

        assert_eq!(entry.level, LogLevel::Info);
        assert_eq!(entry.component, "runner");
        assert_eq!(entry.event, "test_started");
        assert_eq!(entry.installer, Some("nodejs".to_string()));
        assert_eq!(entry.correlation_id, Some("run-123".to_string()));
    }

    #[test]
    fn test_jsonl_reporter_filtering() {
        let file = NamedTempFile::new().unwrap();
        let mut reporter = JsonlReporter::new(file.path(), LogLevel::Warn).unwrap();

        // Debug should be filtered out
        reporter.log(LogEntry::debug("test", "debug_event")).unwrap();
        // Warn should be included
        reporter.log(LogEntry::warn("test", "warn_event")).unwrap();
        // Error should be included
        reporter.log(LogEntry::error("test", "error_event")).unwrap();

        reporter.flush().unwrap();

        let content = std::fs::read_to_string(file.path()).unwrap();
        assert!(!content.contains("debug_event"));
        assert!(content.contains("warn_event"));
        assert!(content.contains("error_event"));
    }

    #[test]
    fn test_log_level_ordering() {
        assert!(LogLevel::Error > LogLevel::Warn);
        assert!(LogLevel::Warn > LogLevel::Info);
        assert!(LogLevel::Info > LogLevel::Debug);
        assert!(LogLevel::Debug > LogLevel::Trace);
    }

    #[test]
    fn test_log_rotation_current_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rotation = LogRotation::new(tmp.path(), 7, "checker");

        let path = rotation.current_log_path();
        let expected_date = chrono::Utc::now().format("%Y%m%d").to_string();

        assert!(path.to_string_lossy().contains(&expected_date));
        assert!(path.to_string_lossy().contains("checker_"));
        assert!(path.to_string_lossy().ends_with(".jsonl"));
    }

    #[test]
    fn test_log_rotation_prune() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rotation = LogRotation::new(tmp.path(), 7, "checker");

        // Create an old log file (simulate 10 days ago)
        let old_date = (chrono::Utc::now() - chrono::Duration::days(10))
            .format("%Y%m%d")
            .to_string();
        let old_file = tmp.path().join(format!("checker_{}.jsonl", old_date));
        std::fs::write(&old_file, "{}").unwrap();

        // Create a recent log file (today)
        let today = chrono::Utc::now().format("%Y%m%d").to_string();
        let today_file = tmp.path().join(format!("checker_{}.jsonl", today));
        std::fs::write(&today_file, "{}").unwrap();

        // Prune
        let deleted = rotation.prune_old_logs().unwrap();

        assert_eq!(deleted, 1);
        assert!(!old_file.exists());
        assert!(today_file.exists());
    }

    #[test]
    fn test_log_rotation_list_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rotation = LogRotation::new(tmp.path(), 7, "checker");

        // Create some log files
        std::fs::write(tmp.path().join("checker_20260125.jsonl"), "{}").unwrap();
        std::fs::write(tmp.path().join("checker_20260126.jsonl"), "{}").unwrap();
        std::fs::write(tmp.path().join("checker_20260127.jsonl"), "{}").unwrap();
        std::fs::write(tmp.path().join("other_file.txt"), "{}").unwrap(); // Should be ignored

        let files = rotation.list_log_files().unwrap();

        assert_eq!(files.len(), 3);
        // Should be sorted newest first
        assert!(files[0].to_string_lossy().contains("20260127"));
        assert!(files[1].to_string_lossy().contains("20260126"));
        assert!(files[2].to_string_lossy().contains("20260125"));
    }
}
