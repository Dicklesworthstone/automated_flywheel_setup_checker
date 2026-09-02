//! Individual installer test execution
//!
//! Data types shared by both execution backends. A [`TestResult`] always carries the full
//! attempt history (`attempts`), a classification on failure (`error`), and the checksum
//! verification state, so every reporting sink renders the same facts.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::parser::ErrorClassification;

/// Status of a test execution (serialized lowercase: `passed`, `timedout`, ... — the same
/// spelling as persisted result entries and `as_str()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus {
    Pending,
    Running,
    Passed,
    Failed,
    Skipped,
    TimedOut,
    /// The run was cancelled (signal, fail-fast, or deadline) before this test finished.
    Cancelled,
}

impl TestStatus {
    /// Lower-case name used in persisted results and human output.
    pub fn as_str(&self) -> &'static str {
        match self {
            TestStatus::Pending => "pending",
            TestStatus::Running => "running",
            TestStatus::Passed => "passed",
            TestStatus::Failed => "failed",
            TestStatus::Skipped => "skipped",
            TestStatus::TimedOut => "timedout",
            TestStatus::Cancelled => "cancelled",
        }
    }
}

/// Information about a retry attempt (legacy view, derived from `attempts`)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryInfo {
    pub attempt: u32,
    pub error: String,
    pub wait_ms: u64,
}

/// One execution attempt of an installer test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// 1-based attempt index
    pub index: u32,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: TestStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// Last 1 KB of stderr for this attempt
    pub stderr_tail: String,
    /// Backoff waited before this attempt started (0 for the first attempt)
    pub waited_before_ms: u64,
}

/// Checksum verification state of an installer test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumState {
    /// No sha256 pinned for this installer; execution was not gated
    #[default]
    NotChecked,
    /// Download hashed and matched the pin; installer was executed
    Verified,
    /// Download hashed and did not match; installer was NOT executed
    Mismatch,
    /// The download failed before verification could happen
    Unknown,
}

impl ChecksumState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChecksumState::NotChecked => "not_checked",
            ChecksumState::Verified => "verified",
            ChecksumState::Mismatch => "mismatch",
            ChecksumState::Unknown => "unknown",
        }
    }
}

/// Result of checksum verification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChecksumResult {
    pub matches: bool,
    pub expected: String,
    pub actual: String,
    pub url: String,
    pub download_ms: u64,
    pub size_bytes: u64,
}

/// Result of an installer test
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub installer_name: String,
    pub status: TestStatus,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Total wall time including retry waits
    pub duration: Duration,
    /// Total wall time in milliseconds including retry waits
    pub duration_ms: u64,
    /// Duration of the final attempt only
    #[serde(default)]
    pub last_attempt_ms: u64,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    /// Index of the final attempt (1-based)
    pub attempt: u32,
    pub max_attempts: u32,
    /// Legacy retry records (one per wait between attempts)
    pub retries: Vec<RetryInfo>,
    /// Full attempt history (empty only for synthetic results)
    #[serde(default)]
    pub attempts: Vec<AttemptRecord>,
    pub container_id: Option<String>,
    pub checksum_result: Option<ChecksumResult>,
    /// Verification state (independent of whether the installer later failed)
    #[serde(default)]
    pub checksum_state: ChecksumState,
    /// Classification, always present when the test did not pass
    pub error: Option<ErrorClassification>,
    /// First line of `version_cmd` output after a passing install
    #[serde(default)]
    pub installed_version: Option<String>,
}

impl TestResult {
    pub fn new(installer_name: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            installer_name: installer_name.into(),
            status: TestStatus::Pending,
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            duration: Duration::ZERO,
            duration_ms: 0,
            last_attempt_ms: 0,
            started_at: now,
            finished_at: now,
            attempt: 1,
            max_attempts: 3,
            retries: Vec::new(),
            attempts: Vec::new(),
            container_id: None,
            checksum_result: None,
            checksum_state: ChecksumState::NotChecked,
            error: None,
            installed_version: None,
        }
    }

    pub fn passed(mut self) -> Self {
        self.status = TestStatus::Passed;
        self.success = true;
        self.exit_code = Some(0);
        self.finished_at = Utc::now();
        self.duration = (self.finished_at - self.started_at).to_std().unwrap_or(Duration::ZERO);
        self.duration_ms = self.duration.as_millis() as u64;
        self.last_attempt_ms = self.duration_ms;
        self
    }

    pub fn failed(mut self, exit_code: i32, stderr: impl Into<String>) -> Self {
        self.status = TestStatus::Failed;
        self.success = false;
        self.exit_code = Some(exit_code);
        self.stderr = stderr.into();
        self.finished_at = Utc::now();
        self.duration = (self.finished_at - self.started_at).to_std().unwrap_or(Duration::ZERO);
        self.duration_ms = self.duration.as_millis() as u64;
        self.last_attempt_ms = self.duration_ms;
        self
    }

    pub fn timed_out(mut self) -> Self {
        self.status = TestStatus::TimedOut;
        self.success = false;
        self.finished_at = Utc::now();
        self.duration = (self.finished_at - self.started_at).to_std().unwrap_or(Duration::ZERO);
        self.duration_ms = self.duration.as_millis() as u64;
        self.last_attempt_ms = self.duration_ms;
        self
    }

    pub fn skipped(mut self, reason: impl Into<String>) -> Self {
        self.status = TestStatus::Skipped;
        self.success = false;
        self.stderr = reason.into();
        self.finished_at = Utc::now();
        self.duration = Duration::ZERO;
        self.duration_ms = 0;
        self.last_attempt_ms = 0;
        self
    }

    pub fn cancelled(mut self, reason: impl Into<String>) -> Self {
        self.status = TestStatus::Cancelled;
        self.success = false;
        self.stderr = reason.into();
        self.finished_at = Utc::now();
        self.duration = (self.finished_at - self.started_at).to_std().unwrap_or(Duration::ZERO);
        self.duration_ms = self.duration.as_millis() as u64;
        self.last_attempt_ms = self.duration_ms;
        self
    }

    pub fn with_container_id(mut self, container_id: impl Into<String>) -> Self {
        self.container_id = Some(container_id.into());
        self
    }

    pub fn with_checksum_result(mut self, result: ChecksumResult) -> Self {
        self.checksum_state =
            if result.matches { ChecksumState::Verified } else { ChecksumState::Mismatch };
        self.checksum_result = Some(result);
        self
    }

    pub fn with_checksum_state(mut self, state: ChecksumState) -> Self {
        self.checksum_state = state;
        self
    }

    pub fn with_error(mut self, error: ErrorClassification) -> Self {
        self.error = Some(error);
        self
    }

    /// Record a retry wait (legacy API; also used by tests). Increments `attempt`.
    pub fn add_retry(&mut self, error: impl Into<String>, wait_ms: u64) {
        self.retries.push(RetryInfo { attempt: self.attempt, error: error.into(), wait_ms });
        self.attempt += 1;
    }

    /// Number of retries performed (attempts beyond the first).
    pub fn retry_count(&self) -> u32 {
        if self.attempts.len() > 1 {
            (self.attempts.len() - 1) as u32
        } else {
            self.retries.len() as u32
        }
    }

    /// Whether the checksum was verified successfully before execution.
    pub fn checksum_verified(&self) -> bool {
        self.checksum_result.as_ref().map(|c| c.matches).unwrap_or(false)
    }
}

/// Configuration for an installer test
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallerTest {
    pub name: String,
    pub url: String,
    pub expected_sha256: Option<String>,
    pub script_path: Option<String>,
    pub timeout: Duration,
    pub timeout_seconds: u64,
    /// Maximum number of attempts (first attempt plus retries)
    pub retry_count: u32,
    pub tags: Vec<String>,
    pub environment: Vec<(String, String)>,
    /// Shell that runs the staged script (`bash` or `sh`)
    #[serde(default = "default_interpreter")]
    pub interpreter: String,
    /// Arguments passed to the installer script
    #[serde(default)]
    pub args: Vec<String>,
    /// Binary that must be on PATH after a passing install
    #[serde(default)]
    pub expect_binary: Option<String>,
    /// Command run after a passing install; non-zero exit fails the test (`post_install`)
    #[serde(default)]
    pub verify_cmd: Option<String>,
    /// Command whose first output line is recorded as `installed_version`
    #[serde(default)]
    pub version_cmd: Option<String>,
    /// Per-installer container memory limit in bytes
    #[serde(default)]
    pub memory_limit: Option<u64>,
    /// Per-installer container network mode (`bridge` or `none`)
    #[serde(default)]
    pub network: Option<String>,
    /// Run as root inside the container
    #[serde(default)]
    pub run_as_root: Option<bool>,
}

fn default_interpreter() -> String {
    "bash".to_string()
}

impl InstallerTest {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            expected_sha256: None,
            script_path: None,
            timeout: Duration::from_secs(300),
            timeout_seconds: 300,
            retry_count: 3,
            tags: Vec::new(),
            environment: Vec::new(),
            interpreter: default_interpreter(),
            args: Vec::new(),
            expect_binary: None,
            verify_cmd: None,
            version_cmd: None,
            memory_limit: None,
            network: None,
            run_as_root: None,
        }
    }

    pub fn with_interpreter(mut self, interpreter: impl Into<String>) -> Self {
        self.interpreter = interpreter.into();
        self
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    pub fn with_expect_binary(mut self, binary: impl Into<String>) -> Self {
        self.expect_binary = Some(binary.into());
        self
    }

    pub fn with_verify_cmd(mut self, cmd: impl Into<String>) -> Self {
        self.verify_cmd = Some(cmd.into());
        self
    }

    pub fn with_version_cmd(mut self, cmd: impl Into<String>) -> Self {
        self.version_cmd = Some(cmd.into());
        self
    }

    pub fn with_memory_limit(mut self, bytes: u64) -> Self {
        self.memory_limit = Some(bytes);
        self
    }

    pub fn with_network(mut self, mode: impl Into<String>) -> Self {
        self.network = Some(mode.into());
        self
    }

    pub fn with_run_as_root(mut self, root: bool) -> Self {
        self.run_as_root = Some(root);
        self
    }

    pub fn with_script_path(mut self, path: impl Into<String>) -> Self {
        self.script_path = Some(path.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self.timeout_seconds = timeout.as_secs();
        self
    }

    pub fn with_sha256(mut self, sha256: impl Into<String>) -> Self {
        self.expected_sha256 = Some(sha256.into());
        self
    }

    /// Set the maximum number of attempts (first attempt plus retries).
    pub fn with_retry_count(mut self, count: u32) -> Self {
        self.retry_count = count;
        self
    }

    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.push((key.into(), value.into()));
        self
    }
}

/// Default cap on captured stdout/stderr bytes per attempt (4 MiB).
pub const DEFAULT_MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;

/// Convert captured bytes to a string bounded by `max_bytes`: when exceeded, keep the first
/// quarter and the last three quarters with a truncation marker in between, so both the
/// installer's banner and its final error survive.
pub fn bound_capture(buf: &[u8], max_bytes: usize) -> String {
    if max_bytes == 0 || buf.len() <= max_bytes {
        return String::from_utf8_lossy(buf).to_string();
    }
    let head_len = max_bytes / 4;
    let tail_len = max_bytes - head_len;
    let dropped = buf.len() - head_len - tail_len;
    let head = String::from_utf8_lossy(&buf[..head_len]);
    let tail_part = String::from_utf8_lossy(&buf[buf.len() - tail_len..]);
    format!("{head}\n[afsc: truncated {dropped} bytes of output]\n{tail_part}")
}

/// Return the last `max_bytes` of `text` on a char boundary.
pub fn tail(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_result_passed() {
        let result = TestResult::new("test-installer").passed();
        assert_eq!(result.status, TestStatus::Passed);
        assert!(result.success);
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn test_result_failed() {
        let result = TestResult::new("test-installer").failed(1, "some error");
        assert_eq!(result.status, TestStatus::Failed);
        assert!(!result.success);
        assert_eq!(result.exit_code, Some(1));
        assert_eq!(result.stderr, "some error");
    }

    #[test]
    fn test_result_retries() {
        let mut result = TestResult::new("test-installer");
        result.add_retry("first failure", 1000);
        result.add_retry("second failure", 2000);

        assert_eq!(result.retry_count(), 2);
        assert_eq!(result.attempt, 3);
        assert_eq!(result.retries[0].wait_ms, 1000);
        assert_eq!(result.retries[1].wait_ms, 2000);
    }

    #[test]
    fn test_retry_count_prefers_attempt_history() {
        let mut result = TestResult::new("x");
        let now = Utc::now();
        for i in 1..=3 {
            result.attempts.push(AttemptRecord {
                index: i,
                started_at: now,
                finished_at: now,
                status: TestStatus::Failed,
                exit_code: Some(1),
                duration_ms: 1,
                stderr_tail: String::new(),
                waited_before_ms: 0,
            });
        }
        assert_eq!(result.retry_count(), 2);
    }

    #[test]
    fn test_installer_test_builder() {
        let test = InstallerTest::new("my-installer", "https://example.com/install.sh")
            .with_sha256("abc123")
            .with_timeout(Duration::from_secs(600))
            .with_retry_count(5)
            .with_tags(vec!["essential".to_string(), "network".to_string()]);

        assert_eq!(test.name, "my-installer");
        assert_eq!(test.expected_sha256, Some("abc123".to_string()));
        assert_eq!(test.timeout_seconds, 600);
        assert_eq!(test.retry_count, 5);
        assert_eq!(test.tags.len(), 2);
    }

    #[test]
    fn test_tail_respects_char_boundaries() {
        let s = "héllo wörld";
        let t = tail(s, 5);
        assert!(s.ends_with(&t));
        assert!(t.len() <= 5);
        assert_eq!(tail("abc", 10), "abc");
    }

    #[test]
    fn test_bound_capture_keeps_head_and_tail() {
        let data: Vec<u8> = (0..10_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let bounded = bound_capture(&data, 1_000);
        assert!(bounded.contains("[afsc: truncated 9000 bytes of output]"));
        assert!(bounded.starts_with(&String::from_utf8_lossy(&data[..250]).to_string()));
        assert!(bounded.ends_with(&String::from_utf8_lossy(&data[data.len() - 750..]).to_string()));
        assert_eq!(bound_capture(b"small", 1_000), "small");
        assert_eq!(bound_capture(b"unbounded", 0), "unbounded");
    }

    #[test]
    fn test_status_as_str() {
        assert_eq!(TestStatus::TimedOut.as_str(), "timedout");
        assert_eq!(TestStatus::Cancelled.as_str(), "cancelled");
    }
}
