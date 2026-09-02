//! Structured JSONL logging and result persistence
//!
//! Result files live under `<data_dir>/results/` and are named
//! `results_<UTC ms timestamp>_<run_id prefix>.jsonl`, so concurrent or same-second runs never
//! collide. Each file holds one `kind: "run"` header line, one `kind: "result"` line per
//! installer, and a final `kind: "summary"` line. Files written by older versions (no `kind`
//! field, second-resolution names) are still readable.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::runner::tail;

/// Bytes of stdout/stderr kept per persisted result.
const PERSISTED_TAIL_BYTES: usize = 2048;

/// Log level for structured entries
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
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
    log_dir: PathBuf,
    retention_days: u32,
    file_prefix: String,
}

impl LogRotation {
    /// Create a new log rotation manager
    pub fn new(
        log_dir: impl Into<PathBuf>,
        retention_days: u32,
        file_prefix: impl Into<String>,
    ) -> Self {
        Self { log_dir: log_dir.into(), retention_days, file_prefix: file_prefix.into() }
    }

    /// Get the path for today's log file
    pub fn current_log_path(&self) -> PathBuf {
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
    pub fn list_log_files(&self) -> Result<Vec<PathBuf>> {
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

/// Persists test run results to JSONL files for later retrieval by the status command.
///
/// Results are written atomically (to a .tmp file, then renamed).
pub struct ResultPersister {
    results_dir: PathBuf,
}

/// Run header: the first line of every results file, describing how the run was executed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RunHeader {
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub tool_version: String,
    pub backend: String,
    pub image: Option<String>,
    pub user: Option<String>,
    pub parallel: usize,
    pub timeout_seconds: u64,
    pub retries: u32,
    pub fail_fast: bool,
    pub acfs_repo: String,
    pub checksums_sha256: Option<String>,
    pub config_source: Option<String>,
    pub data_dir: String,
    pub installers_requested: Vec<String>,
    pub installer_count: usize,
    pub dry_run: bool,
}

impl RunHeader {
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            started_at: Utc::now(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            ..Default::default()
        }
    }
}

/// A single test result line in the results JSONL file
#[derive(Debug, Serialize, Deserialize)]
pub struct ResultEntry {
    pub timestamp: DateTime<Utc>,
    pub installer_name: String,
    pub status: String,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub error_classification: Option<ErrorClassificationEntry>,
    pub stderr_excerpt: String,
    pub retry_count: u32,
    pub sha256_verified: bool,
    /// Verification state: not_checked, verified, mismatch, unknown
    #[serde(default)]
    pub checksum_state: String,
    #[serde(default)]
    pub checksum_expected: Option<String>,
    #[serde(default)]
    pub checksum_actual: Option<String>,
    /// Last 2 KB of stdout
    #[serde(default)]
    pub stdout_tail: String,
    /// Last 2 KB of stderr
    #[serde(default)]
    pub stderr_tail: String,
    #[serde(default)]
    pub container_id: Option<String>,
    /// Duration of the final attempt only (total is `duration_ms`)
    #[serde(default)]
    pub last_attempt_ms: u64,
    /// One entry per attempt, oldest first
    #[serde(default)]
    pub attempts: Vec<AttemptEntry>,
}

/// Persisted view of one execution attempt
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptEntry {
    pub index: u32,
    pub status: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub waited_before_ms: u64,
    pub stderr_tail: String,
}

/// Error classification summary for result entries
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorClassificationEntry {
    pub category: String,
    pub severity: String,
    pub retryable: bool,
    pub confidence: f64,
}

/// Summary line written as the last entry in a results file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummaryEntry {
    pub run_id: String,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    #[serde(default)]
    pub timed_out: usize,
    #[serde(default)]
    pub cancelled: usize,
    pub duration_total_ms: u64,
    pub timestamp_start: DateTime<Utc>,
    pub timestamp_end: DateTime<Utc>,
    /// True when the run was cancelled before every installer finished
    #[serde(default)]
    pub interrupted: bool,
    #[serde(default)]
    pub exit_code: i32,
}

/// Lightweight description of a persisted run (for `status --list`).
#[derive(Debug, Clone, Serialize)]
pub struct RunInfo {
    pub path: PathBuf,
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub interrupted: bool,
}

/// Everything read back from one results file.
#[derive(Debug, Default)]
pub struct RunFile {
    pub header: Option<RunHeader>,
    pub entries: Vec<ResultEntry>,
    pub summary: Option<RunSummaryEntry>,
}

impl ResultPersister {
    /// Create a new ResultPersister with the given results directory
    pub fn new(results_dir: impl Into<PathBuf>) -> Self {
        Self { results_dir: results_dir.into() }
    }

    /// Create with the default results directory (~/.local/share/afsc/results/)
    pub fn default_dir() -> Self {
        let dir = dirs_default_results_dir();
        Self { results_dir: dir }
    }

    /// Ensure the results directory exists
    fn ensure_dir(&self) -> Result<()> {
        if !self.results_dir.exists() {
            std::fs::create_dir_all(&self.results_dir)?;
        }
        Ok(())
    }

    /// Generate the results filename for this run: millisecond UTC timestamp plus run id prefix.
    fn results_filename(&self, run_id: &str) -> String {
        let timestamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        let short: String = run_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
        format!("results_{}_{}.jsonl", timestamp, short)
    }

    /// Write test results to a JSONL file atomically (minimal header).
    ///
    /// Returns the path to the written file.
    pub fn persist(
        &self,
        results: &[crate::runner::TestResult],
        run_id: &str,
        started_at: DateTime<Utc>,
    ) -> Result<PathBuf> {
        let header = RunHeader { started_at, ..RunHeader::new(run_id) };
        self.persist_with_header(results, &header, false)
    }

    /// Write test results with a full run header. `interrupted` marks a cancelled run.
    pub fn persist_with_header(
        &self,
        results: &[crate::runner::TestResult],
        header: &RunHeader,
        interrupted: bool,
    ) -> Result<PathBuf> {
        self.ensure_dir()?;

        let filename = self.results_filename(&header.run_id);
        let final_path = self.results_dir.join(&filename);
        let tmp_path = self.results_dir.join(format!("{}.tmp", filename));

        // Write to temp file
        let file = std::fs::File::create(&tmp_path)?;
        let mut writer = BufWriter::new(file);

        let mut header_json = serde_json::to_value(header)?;
        header_json["kind"] = serde_json::json!("run");
        writeln!(writer, "{}", serde_json::to_string(&header_json)?)?;

        for result in results {
            let entry = Self::entry_from(result);
            let mut json = serde_json::to_value(&entry)?;
            json["kind"] = serde_json::json!("result");
            writeln!(writer, "{}", serde_json::to_string(&json)?)?;
        }

        // Write summary line
        let passed = results.iter().filter(|r| r.success).count();
        let failed = results
            .iter()
            .filter(|r| !r.success && !matches!(r.status, crate::runner::TestStatus::Skipped))
            .count();
        let skipped = results
            .iter()
            .filter(|r| matches!(r.status, crate::runner::TestStatus::Skipped))
            .count();
        let timed_out = results
            .iter()
            .filter(|r| matches!(r.status, crate::runner::TestStatus::TimedOut))
            .count();
        let cancelled = results
            .iter()
            .filter(|r| matches!(r.status, crate::runner::TestStatus::Cancelled))
            .count();
        let total_ms: u64 = results.iter().map(|r| r.duration_ms).sum();

        let summary = RunSummaryEntry {
            run_id: header.run_id.clone(),
            total: results.len(),
            passed,
            failed,
            skipped,
            timed_out,
            cancelled,
            duration_total_ms: total_ms,
            timestamp_start: header.started_at,
            timestamp_end: Utc::now(),
            interrupted,
            exit_code: if results.iter().any(|r| !r.success) { 1 } else { 0 },
        };
        let mut json = serde_json::to_value(&summary)?;
        json["kind"] = serde_json::json!("summary");
        writeln!(writer, "{}", serde_json::to_string(&json)?)?;

        writer.flush()?;
        drop(writer);

        // Atomic rename
        std::fs::rename(&tmp_path, &final_path)?;

        tracing::info!(
            path = %final_path.display(),
            total = results.len(),
            passed = passed,
            failed = failed,
            "Test results persisted"
        );

        Ok(final_path)
    }

    /// Build the persisted entry for a result.
    pub fn entry_from(result: &crate::runner::TestResult) -> ResultEntry {
        ResultEntry {
            timestamp: result.finished_at,
            installer_name: result.installer_name.clone(),
            status: result.status.as_str().to_string(),
            duration_ms: result.duration_ms,
            exit_code: result.exit_code,
            error_classification: result.error.as_ref().map(|e| ErrorClassificationEntry {
                category: e.category.clone(),
                severity: format!("{:?}", e.severity),
                retryable: e.retryable,
                confidence: e.confidence,
            }),
            stderr_excerpt: result.stderr.chars().take(500).collect(),
            retry_count: result.retry_count(),
            sha256_verified: result.checksum_state == crate::runner::ChecksumState::Verified,
            checksum_state: result.checksum_state.as_str().to_string(),
            checksum_expected: result.checksum_result.as_ref().map(|c| c.expected.clone()),
            checksum_actual: result.checksum_result.as_ref().map(|c| c.actual.clone()),
            stdout_tail: tail(&result.stdout, PERSISTED_TAIL_BYTES),
            stderr_tail: tail(&result.stderr, PERSISTED_TAIL_BYTES),
            container_id: result.container_id.clone(),
            last_attempt_ms: result.last_attempt_ms,
            attempts: result
                .attempts
                .iter()
                .map(|a| AttemptEntry {
                    index: a.index,
                    status: a.status.as_str().to_string(),
                    exit_code: a.exit_code,
                    duration_ms: a.duration_ms,
                    waited_before_ms: a.waited_before_ms,
                    stderr_tail: a.stderr_tail.clone(),
                })
                .collect(),
        }
    }

    /// All result files in the directory (any order).
    fn result_files(&self) -> Result<Vec<PathBuf>> {
        if !self.results_dir.exists() {
            return Ok(Vec::new());
        }
        Ok(std::fs::read_dir(&self.results_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("results_") && n.ends_with(".jsonl"))
                    .unwrap_or(false)
            })
            .map(|e| e.path())
            .collect())
    }

    /// Best-effort start time for a results file that has no readable header or summary:
    /// the timestamp embedded in the file name, else the file's modification time.
    fn started_at_from_path(path: &Path) -> DateTime<Utc> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let stamp = name.trim_start_matches("results_").split('_').next().unwrap_or("");
        for fmt in ["%Y%m%dT%H%M%S%.3fZ", "%Y%m%dT%H%M%SZ"] {
            if let Ok(t) = chrono::NaiveDateTime::parse_from_str(stamp, fmt) {
                return t.and_utc();
            }
        }
        // Legacy `results_YYYYmmdd_HHMMSS.jsonl`
        let legacy = name.trim_start_matches("results_").trim_end_matches(".jsonl");
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(legacy, "%Y%m%d_%H%M%S") {
            return t.and_utc();
        }
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| DateTime::<Utc>::from(std::time::UNIX_EPOCH))
    }

    /// List persisted runs, newest first (by header start time, then file name).
    pub fn list_runs(&self) -> Result<Vec<RunInfo>> {
        let mut runs = Vec::new();
        for path in self.result_files()? {
            let Ok(file) = Self::read_run_file(&path) else { continue };
            let (run_id, started_at) = match (&file.header, &file.summary) {
                (Some(h), _) => (h.run_id.clone(), h.started_at),
                (None, Some(s)) => (s.run_id.clone(), s.timestamp_start),
                (None, None) => (String::new(), Self::started_at_from_path(&path)),
            };
            let (total, passed, failed, interrupted) = match &file.summary {
                Some(s) => (s.total, s.passed, s.failed, s.interrupted),
                None => (file.entries.len(), 0, 0, false),
            };
            runs.push(RunInfo { path, run_id, started_at, total, passed, failed, interrupted });
        }
        runs.sort_by(|a, b| b.started_at.cmp(&a.started_at).then_with(|| b.path.cmp(&a.path)));
        Ok(runs)
    }

    /// Get the most recent results file
    pub fn latest_results(&self) -> Result<Option<PathBuf>> {
        Ok(self.list_runs()?.into_iter().next().map(|r| r.path))
    }

    /// Find a run by run id prefix (or `last`).
    pub fn find_run(&self, prefix: &str) -> Result<Option<RunInfo>> {
        let runs = self.list_runs()?;
        if prefix.eq_ignore_ascii_case("last") {
            return Ok(runs.into_iter().next());
        }
        Ok(runs.into_iter().find(|r| r.run_id.starts_with(prefix)))
    }

    /// Keep the newest `keep` result files, delete older ones. `keep == 0` keeps everything.
    /// Only files matching `results_*.jsonl` are ever removed.
    pub fn prune(&self, keep: usize) -> Result<usize> {
        if keep == 0 {
            return Ok(0);
        }
        let runs = self.list_runs()?;
        let mut deleted = 0;
        for run in runs.into_iter().skip(keep) {
            tracing::info!(path = %run.path.display(), "Pruning old results file");
            std::fs::remove_file(&run.path)?;
            deleted += 1;
        }
        Ok(deleted)
    }

    /// Read a results file (header, entries, summary). Old files without `kind` are handled.
    pub fn read_run_file(path: &Path) -> Result<RunFile> {
        let content = std::fs::read_to_string(path)?;
        let mut file = RunFile::default();

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match value.get("kind").and_then(|k| k.as_str()) {
                Some("run") => {
                    if let Ok(h) = serde_json::from_value::<RunHeader>(value.clone()) {
                        file.header = Some(h);
                    }
                }
                Some("result") => {
                    if let Ok(e) = serde_json::from_value::<ResultEntry>(value.clone()) {
                        file.entries.push(e);
                    }
                }
                Some("summary") => {
                    if let Ok(s) = serde_json::from_value::<RunSummaryEntry>(value.clone()) {
                        file.summary = Some(s);
                    }
                }
                _ => {
                    // Legacy line without a kind: summary has run_id, results have installer_name.
                    if let Ok(s) = serde_json::from_value::<RunSummaryEntry>(value.clone()) {
                        file.summary = Some(s);
                    } else if let Ok(e) = serde_json::from_value::<ResultEntry>(value.clone()) {
                        file.entries.push(e);
                    }
                }
            }
        }

        Ok(file)
    }

    /// Read results from a file, returning (entries, summary)
    pub fn read_results(path: &Path) -> Result<(Vec<ResultEntry>, Option<RunSummaryEntry>)> {
        let file = Self::read_run_file(path)?;
        Ok((file.entries, file.summary))
    }

    /// Get the results directory path
    pub fn results_dir(&self) -> &Path {
        &self.results_dir
    }
}

/// Default results directory
fn dirs_default_results_dir() -> PathBuf {
    crate::config::default_data_dir().join("results")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::TestResult;
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
        let old_date =
            (chrono::Utc::now() - chrono::Duration::days(10)).format("%Y%m%d").to_string();
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

    #[test]
    fn test_same_millisecond_persists_do_not_collide() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = ResultPersister::new(tmp.path());
        let results = vec![TestResult::new("a").passed()];
        let now = Utc::now();
        let a = p.persist(&results, "11111111-aaaa", now).unwrap();
        let b = p.persist(&results, "22222222-bbbb", now).unwrap();
        assert_ne!(a, b);
        assert!(a.exists() && b.exists());
        assert_eq!(p.list_runs().unwrap().len(), 2);
    }

    #[test]
    fn test_run_file_round_trip_with_header_and_kinds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = ResultPersister::new(tmp.path());
        let mut failed = TestResult::new("b").failed(100, "E: Unable to locate package foo");
        failed.stdout = "some stdout".into();
        crate::runner::finalize_failure(&mut failed, None);
        let results = vec![TestResult::new("a").passed(), failed];
        let header = RunHeader {
            backend: "local".into(),
            parallel: 2,
            timeout_seconds: 60,
            retries: 1,
            installer_count: 2,
            ..RunHeader::new("run-xyz")
        };
        let path = p.persist_with_header(&results, &header, true).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let kinds: Vec<String> = content
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, vec!["run", "result", "result", "summary"]);

        let file = ResultPersister::read_run_file(&path).unwrap();
        let h = file.header.unwrap();
        assert_eq!(h.run_id, "run-xyz");
        assert_eq!(h.parallel, 2);
        assert_eq!(file.entries.len(), 2);
        let b = &file.entries[1];
        assert_eq!(b.status, "failed");
        assert_eq!(b.stdout_tail, "some stdout");
        assert_eq!(b.error_classification.as_ref().unwrap().category, "dependency");
        assert_eq!(b.checksum_state, "not_checked");
        let s = file.summary.unwrap();
        assert!(s.interrupted);
        assert_eq!(s.exit_code, 1);
        assert_eq!(s.failed, 1);
    }

    #[test]
    fn test_legacy_file_without_kind_is_readable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("results_20260901_120000.jsonl");
        let legacy = concat!(
            "{\"timestamp\":\"2026-09-01T12:00:00Z\",\"installer_name\":\"x\",\"status\":\"passed\",\"duration_ms\":5,\"exit_code\":0,\"error_classification\":null,\"stderr_excerpt\":\"\",\"retry_count\":0,\"sha256_verified\":true}\n",
            "{\"run_id\":\"legacy\",\"total\":1,\"passed\":1,\"failed\":0,\"skipped\":0,\"duration_total_ms\":5,\"timestamp_start\":\"2026-09-01T12:00:00Z\",\"timestamp_end\":\"2026-09-01T12:00:01Z\"}\n"
        );
        std::fs::write(&path, legacy).unwrap();
        let file = ResultPersister::read_run_file(&path).unwrap();
        assert!(file.header.is_none());
        assert_eq!(file.entries.len(), 1);
        assert_eq!(file.summary.unwrap().run_id, "legacy");
        let p = ResultPersister::new(tmp.path());
        assert_eq!(p.latest_results().unwrap().unwrap(), path);
    }

    #[test]
    fn test_prune_keeps_newest_and_ignores_foreign_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = ResultPersister::new(tmp.path());
        let results = vec![TestResult::new("a").passed()];
        let base = Utc::now() - chrono::Duration::minutes(10);
        for i in 0..5 {
            p.persist(&results, &format!("run-{i}"), base + chrono::Duration::minutes(i)).unwrap();
        }
        std::fs::write(tmp.path().join("notes.txt"), "keep me").unwrap();
        std::fs::write(tmp.path().join("metrics.json"), "{}").unwrap();

        assert_eq!(p.prune(0).unwrap(), 0);
        assert_eq!(p.prune(2).unwrap(), 3);
        let runs = p.list_runs().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].run_id, "run-4");
        assert_eq!(runs[1].run_id, "run-3");
        assert!(tmp.path().join("notes.txt").exists());
        assert!(tmp.path().join("metrics.json").exists());
    }

    #[test]
    fn test_find_run_by_prefix_and_last() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = ResultPersister::new(tmp.path());
        let results = vec![TestResult::new("a").passed()];
        let t0 = Utc::now() - chrono::Duration::minutes(5);
        p.persist(&results, "abcdef12-old", t0).unwrap();
        p.persist(&results, "fedcba98-new", t0 + chrono::Duration::minutes(1)).unwrap();
        assert_eq!(p.find_run("last").unwrap().unwrap().run_id, "fedcba98-new");
        assert_eq!(p.find_run("abcd").unwrap().unwrap().run_id, "abcdef12-old");
        assert!(p.find_run("zzz").unwrap().is_none());
    }
}
