//! Installer execution logic with Docker and local backends
//!
//! This module implements the core test runner that executes installer scripts
//! either inside Docker containers (default) or in isolated local temp directories.
//!
//! Contract (shared by both backends):
//! - every non-passing result carries an [`ErrorClassification`] (see [`finalize_failure`]);
//! - retries accumulate an [`AttemptRecord`] per attempt instead of replacing the result;
//! - checksum verification runs before execution and a mismatch is never retried.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::parser::{classify_error, ErrorClassification, TIMEOUT_MARKER};

use super::container::{ContainerConfig, ContainerGuard, ContainerManager, PullPolicy};
use super::installer::{
    tail, AttemptRecord, ChecksumResult, InstallerTest, RetryInfo, TestResult, TestStatus,
};
use super::retry::RetryConfig;

const CURL_BIN: &str = "curl";
const BASH_BIN: &str = "bash";

/// Bytes of stdout appended to the classification input.
const CLASSIFY_STDOUT_TAIL_BYTES: usize = 4096;
/// Bytes of stderr kept per attempt record.
const ATTEMPT_STDERR_TAIL_BYTES: usize = 1024;
/// Additive jitter fraction applied to retry backoff.
const RETRY_JITTER_FRACTION: f64 = 0.25;

/// Execution backend selection
#[derive(Debug, Clone)]
pub enum ExecutionBackend {
    /// Run installers inside Docker containers (default, recommended)
    Docker { container_config: ContainerConfig, pull_policy: PullPolicy },
    /// Run installers locally in temp directories (fallback)
    Local,
}

impl Default for ExecutionBackend {
    fn default() -> Self {
        ExecutionBackend::Docker {
            container_config: ContainerConfig::default(),
            pull_policy: PullPolicy::IfNotPresent,
        }
    }
}

/// Configuration for the installer test runner
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// Default timeout for tests
    pub default_timeout: Duration,
    /// Whether to run in dry-run mode (--dry-run flag passed to installer)
    pub dry_run: bool,
    /// Additional environment variables to set
    pub extra_env: Vec<(String, String)>,
    /// Execution backend
    pub backend: ExecutionBackend,
    /// Backoff policy between attempts (the attempt count comes from each `InstallerTest`)
    pub retry: RetryConfig,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(300),
            dry_run: false,
            extra_env: Vec::new(),
            backend: ExecutionBackend::Docker {
                container_config: ContainerConfig::default(),
                pull_policy: PullPolicy::IfNotPresent,
            },
            retry: RetryConfig::executor_default(3),
        }
    }
}

/// Attach a classification to a non-passing result if it does not already have one.
///
/// The classifier sees stderr followed by the last 4 KB of stdout (installers such as srps print
/// their refusal on stdout). `synthetic` lets the executor prepend a marker describing what the
/// runner observed (timeout, cancellation) rather than what the installer printed.
pub fn finalize_failure(result: &mut TestResult, synthetic: Option<&str>) {
    if result.success || result.status == TestStatus::Passed || result.error.is_some() {
        return;
    }
    let stdout_tail = tail(&result.stdout, CLASSIFY_STDOUT_TAIL_BYTES);
    let text = format!("{}\n{}\n{}", synthetic.unwrap_or(""), result.stderr, stdout_tail);
    result.error = Some(classify_error(&text, result.exit_code.unwrap_or(-1)));
}

/// Classify a non-passing result without mutating it (used by callers holding a reference).
pub fn classify_result(result: &TestResult) -> ErrorClassification {
    let stdout_tail = tail(&result.stdout, CLASSIFY_STDOUT_TAIL_BYTES);
    let text = format!("{}\n{}", result.stderr, stdout_tail);
    classify_error(&text, result.exit_code.unwrap_or(-1))
}

/// Executes installer tests in isolated environments
pub struct InstallerTestRunner {
    config: RunnerConfig,
}

impl InstallerTestRunner {
    pub fn new(config: RunnerConfig) -> Self {
        Self { config }
    }

    /// Build a shell script that downloads, verifies checksum, and executes the installer.
    ///
    /// When expected_sha256 is provided, the script:
    ///   1. Downloads to a temp file
    ///   2. Computes SHA256 and compares
    ///   3. Only executes if checksum matches (or exits 99 on mismatch)
    ///
    /// When no checksum is expected, falls back to curl|bash for simplicity.
    fn build_verified_install_script(
        &self,
        url: &str,
        installer_name: &str,
        expected_sha256: Option<&str>,
    ) -> String {
        let flags = self.installer_flags(installer_name);

        match expected_sha256 {
            Some(expected) => {
                // Download → verify checksum → execute via stdin.
                //
                // CRITICAL: set -e is used ONLY for download+verify. We switch to
                // set +e before running the installer because installer scripts handle
                // their own errors internally (e.g., `command -v foo` returning 1 is
                // normal). With set -e, these benign non-zero exits kill the entire
                // script, causing false failures.
                let script_path = format!("/tmp/installer_{}.sh", installer_name);
                format!(
                    r#"set -e
{curl} -fsSL '{url}' -o '{path}'
ACTUAL=$(sha256sum '{path}' | cut -d' ' -f1)
if [ "$ACTUAL" != "{expected}" ]; then
  echo "CHECKSUM_MISMATCH: expected={expected} actual=$ACTUAL url={url}" >&2
  exit 99
fi
echo "CHECKSUM_OK $ACTUAL" >&2
set +e
{bash} -s --{flags} < '{path}'"#,
                    curl = CURL_BIN,
                    url = url,
                    path = script_path,
                    expected = expected,
                    bash = BASH_BIN,
                    flags = flags,
                )
            }
            None => {
                // No checksum — use curl|bash directly
                format!("{CURL_BIN} -fsSL '{url}' | {BASH_BIN} -s --{flags}")
            }
        }
    }

    fn installer_args(installer_name: &str) -> &'static str {
        match installer_name {
            "rust" => "-y --no-modify-path",
            _ => "",
        }
    }

    fn installer_flags(&self, installer_name: &str) -> String {
        let installer_args = Self::installer_args(installer_name);
        match (self.config.dry_run, installer_args.is_empty()) {
            (true, true) => " --dry-run".to_string(),
            (true, false) => format!(" --dry-run {installer_args}"),
            (false, true) => String::new(),
            (false, false) => format!(" {installer_args}"),
        }
    }

    /// Parse checksum result from stderr output.
    ///
    /// The verified script prints `CHECKSUM_OK <hash>` after a successful compare and
    /// `CHECKSUM_MISMATCH: …` (exit 99) otherwise, so verification state is known even when the
    /// installer itself later fails.
    fn parse_checksum_result(
        &self,
        stderr: &str,
        exit_code: i32,
        url: &str,
        expected_sha256: Option<&str>,
        download_ms: u64,
    ) -> Option<ChecksumResult> {
        let expected = expected_sha256?;

        if exit_code == 99 && stderr.contains("CHECKSUM_MISMATCH") {
            // Explicit checksum mismatch — extract actual hash from error message
            let actual = stderr
                .lines()
                .find(|l| l.contains("CHECKSUM_MISMATCH"))
                .and_then(|l| l.split("actual=").nth(1))
                .map(|s| s.split_whitespace().next().unwrap_or("unknown"))
                .unwrap_or("unknown")
                .to_string();

            Some(ChecksumResult {
                matches: false,
                expected: expected.to_string(),
                actual,
                url: url.to_string(),
                download_ms,
                size_bytes: 0,
            })
        } else if exit_code == 0 || stderr.contains("CHECKSUM_OK") {
            // The compare succeeded (marker present) or the whole script succeeded.
            let actual = stderr
                .lines()
                .find_map(|l| l.strip_prefix("CHECKSUM_OK "))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| expected.to_string());
            Some(ChecksumResult {
                matches: true,
                expected: expected.to_string(),
                actual,
                url: url.to_string(),
                download_ms,
                size_bytes: 0,
            })
        } else {
            // Non-zero exit before the marker (download error) — verification state unknown.
            None
        }
    }

    /// Determine timeout for a test
    fn test_timeout(&self, test: &InstallerTest) -> Duration {
        if test.timeout.as_secs() > 0 {
            test.timeout
        } else {
            self.config.default_timeout
        }
    }

    /// Run an installer test using the configured backend (single attempt)
    pub async fn run_test(&self, test: &InstallerTest) -> Result<TestResult> {
        let mut result = match &self.config.backend {
            ExecutionBackend::Docker { container_config, pull_policy } => {
                self.run_test_docker(test, container_config, pull_policy).await?
            }
            ExecutionBackend::Local => self.run_test_local(test).await?,
        };
        // Every backend exit path is covered here so no failure escapes unclassified.
        if !result.success {
            finalize_failure(&mut result, None);
        }
        Ok(result)
    }

    /// Run an installer test inside a Docker container
    async fn run_test_docker(
        &self,
        test: &InstallerTest,
        container_config: &ContainerConfig,
        pull_policy: &PullPolicy,
    ) -> Result<TestResult> {
        let mut result = TestResult::new(&test.name);
        let start_time = Instant::now();
        let test_timeout = self.test_timeout(test);

        info!(
            installer = %test.name,
            url = %test.url,
            backend = "docker",
            "Starting installer test"
        );

        // Build container config with test-specific environment
        let mut config = container_config.clone();
        // Add test-specific environment
        for (key, value) in &test.environment {
            config.environment.push((key.clone(), value.clone()));
        }
        // Add runner extra environment
        for (key, value) in &self.config.extra_env {
            config.environment.push((key.clone(), value.clone()));
        }

        // Create container manager and container
        let manager = ContainerManager::new(config).with_pull_policy(pull_policy.clone());

        let container_id = manager
            .create_container(&test.name)
            .await
            .context("Failed to create Docker container")?;

        // Set up guard for cleanup on early return/panic
        let mut guard = ContainerGuard::new(container_id.clone(), manager.docker_arc());
        result = result.with_container_id(&container_id);

        // Install prerequisite packages if NOT using the pre-built base image.
        // The afsc-base:latest image already has everything pre-installed (Rust, Node,
        // git, unzip, etc.) so we skip this 20-30s step entirely.
        let using_base_image = container_config.image == ContainerManager::AFSC_BASE_IMAGE;
        if !using_base_image {
            debug!(container_id = %container_id, "Installing prerequisites in container (not using base image)");
            let prereq_install_result = timeout(
                Duration::from_secs(180),
                manager.exec_in_container(
                    &container_id,
                    &[
                        "bash",
                        "-c",
                        "apt-get update -qq && apt-get install -y -qq \
                        curl ca-certificates git unzip xz-utils tar jq \
                        build-essential sudo gnupg libssl-dev pkg-config \
                        python3 rsync zsh >/dev/null 2>&1",
                    ],
                ),
            )
            .await;
            match prereq_install_result {
                Ok(Ok((code, _, _))) if code != 0 => {
                    warn!(container_id = %container_id, exit_code = code, "Prerequisite installation exited non-zero");
                }
                Ok(Err(e)) => {
                    warn!(container_id = %container_id, error = %e, "Failed to install prerequisites in container");
                }
                Err(_) => {
                    warn!(container_id = %container_id, "Prerequisite installation timed out after 180s");
                }
                _ => {
                    debug!(container_id = %container_id, "Prerequisites installed successfully");
                }
            }
        } else {
            debug!(container_id = %container_id, "Using pre-built base image — skipping prerequisite installation");
        }

        // Build the verified install script (download → checksum → execute)
        let install_script = self.build_verified_install_script(
            &test.url,
            &test.name,
            test.expected_sha256.as_deref(),
        );
        debug!(
            container_id = %container_id,
            has_checksum = test.expected_sha256.is_some(),
            "Executing installer in container"
        );

        // Execute with timeout
        let exec_result = timeout(
            test_timeout,
            manager.exec_in_container(&container_id, &["bash", "-c", &install_script]),
        )
        .await;

        match exec_result {
            Ok(Ok((exit_code, stdout, stderr))) => {
                let elapsed = start_time.elapsed();
                result.stdout = stdout;
                result.stderr = stderr.clone();
                result.exit_code = Some(exit_code);
                result.duration = elapsed;
                result.duration_ms = elapsed.as_millis() as u64;

                // Parse checksum result
                if let Some(checksum_result) = self.parse_checksum_result(
                    &stderr,
                    exit_code,
                    &test.url,
                    test.expected_sha256.as_deref(),
                    elapsed.as_millis() as u64,
                ) {
                    if !checksum_result.matches {
                        warn!(
                            installer = %test.name,
                            expected = %checksum_result.expected,
                            actual = %checksum_result.actual,
                            "Checksum mismatch — installer NOT executed"
                        );
                        result.status = TestStatus::Failed;
                        result.success = false;
                        result = result.with_checksum_result(checksum_result);
                        // Clean up and return early — do NOT consider this retryable
                        guard.cleanup().await;
                        result.finished_at = chrono::Utc::now();
                        result.last_attempt_ms = result.duration_ms;
                        return Ok(result);
                    }
                    result = result.with_checksum_result(checksum_result);
                }

                if exit_code == 0 {
                    info!(
                        installer = %test.name,
                        container_id = %container_id,
                        duration_ms = elapsed.as_millis(),
                        "Installer test passed"
                    );
                    result.status = TestStatus::Passed;
                    result.success = true;
                } else {
                    warn!(
                        installer = %test.name,
                        container_id = %container_id,
                        exit_code = exit_code,
                        duration_ms = elapsed.as_millis(),
                        "Installer test failed"
                    );
                    result.status = TestStatus::Failed;
                    result.success = false;
                }
            }
            Ok(Err(e)) => {
                let elapsed = start_time.elapsed();
                warn!(
                    installer = %test.name,
                    container_id = %container_id,
                    error = %e,
                    "Installer execution error in container"
                );
                result.stderr = format!("Container execution error: {}", e);
                result.status = TestStatus::Failed;
                result.success = false;
                result.duration = elapsed;
                result.duration_ms = elapsed.as_millis() as u64;
            }
            Err(_) => {
                warn!(
                    installer = %test.name,
                    container_id = %container_id,
                    timeout_seconds = test_timeout.as_secs(),
                    "Installer test timed out in container"
                );
                result.status = TestStatus::TimedOut;
                result.success = false;
                result.stderr = format!(
                    "{TIMEOUT_MARKER} after {}s (Test timed out after {:?})",
                    test_timeout.as_secs(),
                    test_timeout
                );
                result.duration = test_timeout;
                result.duration_ms = test_timeout.as_millis() as u64;
            }
        }

        // Always clean up the container
        guard.cleanup().await;
        result.finished_at = chrono::Utc::now();
        result.last_attempt_ms = result.duration_ms;
        Ok(result)
    }

    /// Run an installer test locally in an isolated temp directory (fallback)
    async fn run_test_local(&self, test: &InstallerTest) -> Result<TestResult> {
        let mut result = TestResult::new(&test.name);
        let start_time = Instant::now();

        info!(
            installer = %test.name,
            url = %test.url,
            backend = "local",
            "Starting installer test"
        );

        // Create isolated temp directory
        let temp_dir = TempDir::new().context("Failed to create temp directory")?;
        let temp_path = temp_dir.path().to_path_buf();
        debug!(path = ?temp_path, "Created temp directory");

        let test_timeout = self.test_timeout(test);

        // If we have an expected checksum, download and verify locally first
        if let Some(expected_sha256) = &test.expected_sha256 {
            let script_file = temp_path.join(format!("installer_{}.sh", test.name));
            let download_start = Instant::now();

            // Download the script
            let dl_output = Command::new(CURL_BIN)
                .args(["-fsSL", &test.url, "-o"])
                .arg(&script_file)
                .output()
                .await
                .context("Failed to download installer script")?;

            let download_ms = download_start.elapsed().as_millis() as u64;

            if !dl_output.status.success() {
                let stderr = String::from_utf8_lossy(&dl_output.stderr).to_string();
                result.stderr = format!("Download failed: {}", stderr);
                result.exit_code = dl_output.status.code();
                result.status = TestStatus::Failed;
                result.success = false;
                result.finished_at = chrono::Utc::now();
                result.duration = start_time.elapsed();
                result.duration_ms = result.duration.as_millis() as u64;
                result.last_attempt_ms = result.duration_ms;
                return Ok(result);
            }

            // Compute SHA256
            let file_bytes =
                tokio::fs::read(&script_file).await.context("Failed to read downloaded script")?;
            let mut hasher = Sha256::new();
            hasher.update(&file_bytes);
            let actual_hash = hex::encode(hasher.finalize());
            let size_bytes = file_bytes.len() as u64;

            let checksum_result = ChecksumResult {
                matches: actual_hash == *expected_sha256,
                expected: expected_sha256.clone(),
                actual: actual_hash.clone(),
                url: test.url.clone(),
                download_ms,
                size_bytes,
            };

            if !checksum_result.matches {
                warn!(
                    installer = %test.name,
                    expected = %expected_sha256,
                    actual = %actual_hash,
                    "Checksum mismatch — installer NOT executed"
                );
                result.stderr = format!(
                    "CHECKSUM_MISMATCH: expected={} actual={} url={}",
                    expected_sha256, actual_hash, test.url
                );
                result.exit_code = Some(99);
                result.status = TestStatus::Failed;
                result.success = false;
                result = result.with_checksum_result(checksum_result);
                result.finished_at = chrono::Utc::now();
                result.duration = start_time.elapsed();
                result.duration_ms = result.duration.as_millis() as u64;
                result.last_attempt_ms = result.duration_ms;
                return Ok(result);
            }

            info!(
                installer = %test.name,
                hash = %actual_hash,
                size = size_bytes,
                "Checksum verified — executing installer"
            );
            result = result.with_checksum_result(checksum_result);
        }

        // Build the execution command.
        // If we already downloaded and verified the script (checksum path above),
        // execute the local file via stdin instead of re-downloading.
        // Using `bash -s -- < file` matches the curl|bash pipe behavior.
        let curl_bash_script = if test.expected_sha256.is_some() {
            let script_file = temp_path.join(format!("installer_{}.sh", test.name));
            let flags = self.installer_flags(&test.name);
            format!("{BASH_BIN} -s --{flags} < '{}'", script_file.display())
        } else {
            self.build_verified_install_script(&test.url, &test.name, None)
        };

        debug!(script = %curl_bash_script, "Executing installer script locally");

        // Create the command
        let mut cmd = Command::new(BASH_BIN);
        cmd.arg("-c")
            .arg(&curl_bash_script)
            .current_dir(&temp_path)
            .env("HOME", &temp_path)
            .env("TMPDIR", &temp_path)
            .env("XDG_CONFIG_HOME", temp_path.join(".config"))
            .env("XDG_DATA_HOME", temp_path.join(".local/share"))
            .env("XDG_CACHE_HOME", temp_path.join(".cache"))
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .env("DEBIAN_FRONTEND", "noninteractive")
            .env("NONINTERACTIVE", "1")
            .env("CI", "true")
            .env("RUSTUP_INIT_SKIP_PATH_CHECK", "yes")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Add test-specific environment variables
        for (key, value) in &test.environment {
            cmd.env(key, value);
        }

        // Add config extra environment variables
        for (key, value) in &self.config.extra_env {
            cmd.env(key, value);
        }

        // Spawn the process
        let mut child = cmd.spawn().context("Failed to spawn installer process")?;

        // Get stdout/stderr handles
        let mut stdout_handle = child.stdout.take().expect("stdout was piped");
        let mut stderr_handle = child.stderr.take().expect("stderr was piped");

        // Read outputs with timeout
        let execution_result = timeout(test_timeout, async {
            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();

            let (stdout_result, stderr_result) = tokio::join!(
                stdout_handle.read_to_end(&mut stdout_buf),
                stderr_handle.read_to_end(&mut stderr_buf)
            );

            stdout_result.context("Failed to read stdout")?;
            stderr_result.context("Failed to read stderr")?;

            let status = child.wait().await.context("Failed to wait for process")?;

            Ok::<_, anyhow::Error>((status, stdout_buf, stderr_buf))
        })
        .await;

        match execution_result {
            Ok(Ok((status, stdout_buf, stderr_buf))) => {
                let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
                let stderr = String::from_utf8_lossy(&stderr_buf).to_string();
                let exit_code = status.code().unwrap_or(-1);
                let elapsed = start_time.elapsed();

                result.stdout = stdout;
                result.stderr = stderr;
                result.exit_code = Some(exit_code);
                result.duration = elapsed;
                result.duration_ms = elapsed.as_millis() as u64;

                if status.success() {
                    info!(
                        installer = %test.name,
                        duration_ms = elapsed.as_millis(),
                        "Installer test passed (local)"
                    );
                    result.status = TestStatus::Passed;
                    result.success = true;
                } else {
                    warn!(
                        installer = %test.name,
                        exit_code = exit_code,
                        duration_ms = elapsed.as_millis(),
                        "Installer test failed (local)"
                    );
                    result.status = TestStatus::Failed;
                    result.success = false;
                }
            }
            Ok(Err(e)) => {
                warn!(installer = %test.name, error = %e, "Installer execution error (local)");
                result.stderr = format!("Execution error: {}", e);
                result.status = TestStatus::Failed;
                result.success = false;
                result.duration = start_time.elapsed();
                result.duration_ms = result.duration.as_millis() as u64;
            }
            Err(_) => {
                warn!(
                    installer = %test.name,
                    timeout_seconds = test_timeout.as_secs(),
                    "Installer test timed out (local)"
                );

                if let Err(e) = child.kill().await {
                    debug!(error = %e, "Failed to kill timed-out process");
                }

                result.status = TestStatus::TimedOut;
                result.success = false;
                result.stderr = format!(
                    "{TIMEOUT_MARKER} after {}s (Test timed out after {:?})",
                    test_timeout.as_secs(),
                    test_timeout
                );
                result.duration = test_timeout;
                result.duration_ms = test_timeout.as_millis() as u64;
            }
        }

        debug!(path = ?temp_path, "Cleaning up temp directory");
        result.finished_at = chrono::Utc::now();
        result.last_attempt_ms = result.duration_ms;
        Ok(result)
    }

    /// Run a test with retries (each retry creates a fresh container in Docker mode).
    ///
    /// The returned result is the final attempt's result enriched with the full attempt history,
    /// the legacy `retries` records, and the total wall time including backoff waits.
    pub async fn run_test_with_retry(&self, test: &InstallerTest) -> Result<TestResult> {
        let overall_start = Instant::now();
        let started_at = chrono::Utc::now();
        let max_attempts = test.retry_count.max(1);

        let mut attempts: Vec<AttemptRecord> = Vec::new();
        let mut retries: Vec<RetryInfo> = Vec::new();
        let mut waited_before_ms: u64 = 0;
        let mut attempt_index: u32 = 1;

        loop {
            let mut result = self.run_test(test).await?;

            attempts.push(AttemptRecord {
                index: attempt_index,
                started_at: result.started_at,
                finished_at: result.finished_at,
                status: result.status,
                exit_code: result.exit_code,
                duration_ms: result.duration_ms,
                stderr_tail: tail(&result.stderr, ATTEMPT_STDERR_TAIL_BYTES),
                waited_before_ms,
            });

            let retry = attempt_index < max_attempts && Self::should_retry_result(&result);
            if !retry {
                result.last_attempt_ms = result.duration_ms;
                result.attempt = attempt_index;
                result.max_attempts = max_attempts;
                result.attempts = attempts;
                result.retries = retries;
                result.started_at = started_at;
                result.duration = overall_start.elapsed();
                result.duration_ms = result.duration.as_millis() as u64;
                return Ok(result);
            }

            let wait = self.config.retry.delay_with_jitter(attempt_index, RETRY_JITTER_FRACTION);
            let wait_ms = wait.as_millis() as u64;
            info!(
                installer = %test.name,
                attempt = attempt_index + 1,
                max_attempts = max_attempts,
                wait_ms = wait_ms,
                "Retrying failed test"
            );
            retries.push(RetryInfo {
                attempt: attempt_index,
                error: tail(&result.stderr, ATTEMPT_STDERR_TAIL_BYTES),
                wait_ms,
            });
            tokio::time::sleep(wait).await;
            waited_before_ms = wait_ms;
            attempt_index += 1;
        }
    }

    /// Retry policy: only transient (retryable) classifications, never checksum mismatches,
    /// timeouts, or cancellations.
    fn should_retry_result(result: &TestResult) -> bool {
        if result.success {
            return false;
        }

        if matches!(result.status, TestStatus::TimedOut | TestStatus::Cancelled | TestStatus::Skipped)
        {
            return false;
        }

        if let Some(checksum_result) = &result.checksum_result {
            if !checksum_result.matches {
                return false;
            }
        }

        match &result.error {
            Some(classification) => classification.retryable,
            None => classify_result(result).retryable,
        }
    }

    pub fn config(&self) -> &RunnerConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runner_config_default() {
        let config = RunnerConfig::default();
        assert_eq!(config.default_timeout, Duration::from_secs(300));
        assert!(!config.dry_run);
        assert!(matches!(config.backend, ExecutionBackend::Docker { .. }));
        assert_eq!(config.retry.max_attempts, 4);
    }

    #[test]
    fn test_runner_config_local_backend() {
        let config = RunnerConfig { backend: ExecutionBackend::Local, ..Default::default() };
        assert!(matches!(config.backend, ExecutionBackend::Local));
    }

    #[test]
    fn test_backoff_calculation() {
        let config = RunnerConfig { backend: ExecutionBackend::Local, ..Default::default() };
        let runner = InstallerTestRunner::new(config);

        let backoff1 = runner.config.retry.delay_with_jitter(1, RETRY_JITTER_FRACTION);
        assert!((2000..=2500).contains(&(backoff1.as_millis() as u64)));

        let backoff2 = runner.config.retry.delay_with_jitter(2, RETRY_JITTER_FRACTION);
        assert!((4000..=5000).contains(&(backoff2.as_millis() as u64)));
    }

    #[test]
    fn test_dry_run_false_does_not_append_flag() {
        // Regression (br-74o.13): When dry_run is false, the curl|bash command
        // must NOT contain --dry-run.
        let config =
            RunnerConfig { dry_run: false, backend: ExecutionBackend::Local, ..Default::default() };
        let runner = InstallerTestRunner::new(config);
        let cmd =
            runner.build_verified_install_script("https://example.com/install.sh", "test", None);
        assert!(
            !cmd.contains("--dry-run"),
            "Command must not contain --dry-run when dry_run=false"
        );
    }

    #[test]
    fn test_dry_run_true_appends_flag() {
        let config =
            RunnerConfig { dry_run: true, backend: ExecutionBackend::Local, ..Default::default() };
        let runner = InstallerTestRunner::new(config);
        let cmd =
            runner.build_verified_install_script("https://example.com/install.sh", "test", None);
        assert!(cmd.contains("--dry-run"), "Command must contain --dry-run when dry_run=true");
    }

    #[test]
    fn test_build_install_command_format() {
        let config =
            RunnerConfig { dry_run: false, backend: ExecutionBackend::Local, ..Default::default() };
        let runner = InstallerTestRunner::new(config);
        let cmd =
            runner.build_verified_install_script("https://example.com/install.sh", "test", None);
        assert!(cmd.contains("curl -fsSL"));
        assert!(cmd.contains("https://example.com/install.sh"));
        assert!(cmd.contains("| bash -s --"));
    }

    #[test]
    fn test_build_verified_script_with_checksum() {
        let config =
            RunnerConfig { dry_run: false, backend: ExecutionBackend::Local, ..Default::default() };
        let runner = InstallerTestRunner::new(config);
        let cmd = runner.build_verified_install_script(
            "https://example.com/install.sh",
            "myinstaller",
            Some("abc123def456"),
        );
        // Should download to temp file, not pipe to bash
        assert!(cmd.contains("-o '/tmp/installer_myinstaller.sh'"));
        assert!(cmd.contains("sha256sum"));
        assert!(cmd.contains("abc123def456"));
        assert!(cmd.contains("CHECKSUM_MISMATCH"));
        assert!(cmd.contains("CHECKSUM_OK"));
        assert!(cmd.contains("exit 99"));
        // Should NOT contain pipe to bash — uses stdin redirect instead
        assert!(!cmd.contains("| bash"));
        // Should execute via stdin redirect for consistent behavior
        assert!(cmd.contains("< '/tmp/installer_myinstaller.sh'"));
    }

    #[test]
    fn test_build_verified_script_without_checksum() {
        let config =
            RunnerConfig { dry_run: false, backend: ExecutionBackend::Local, ..Default::default() };
        let runner = InstallerTestRunner::new(config);
        let cmd = runner.build_verified_install_script(
            "https://example.com/install.sh",
            "myinstaller",
            None,
        );
        // Without checksum, should use curl|bash directly
        assert!(cmd.contains("| bash"));
        assert!(!cmd.contains("sha256sum"));
    }

    #[test]
    fn test_rust_installer_runs_noninteractively() {
        let config =
            RunnerConfig { dry_run: false, backend: ExecutionBackend::Local, ..Default::default() };
        let runner = InstallerTestRunner::new(config);
        let cmd =
            runner.build_verified_install_script("https://sh.rustup.rs", "rust", Some("abc123"));
        assert!(cmd.contains("bash -s -- -y --no-modify-path"));
    }

    #[test]
    fn test_rust_installer_flags_apply_to_verified_local_script() {
        let config =
            RunnerConfig { dry_run: false, backend: ExecutionBackend::Local, ..Default::default() };
        let runner = InstallerTestRunner::new(config);
        let flags = runner.installer_flags("rust");
        assert_eq!(flags, " -y --no-modify-path");
    }

    #[test]
    fn test_parse_checksum_verified_but_installer_failed() {
        let config = RunnerConfig { backend: ExecutionBackend::Local, ..Default::default() };
        let runner = InstallerTestRunner::new(config);
        let stderr = "CHECKSUM_OK abc123\nsome installer error\n";
        let parsed = runner
            .parse_checksum_result(stderr, 1, "https://x/install.sh", Some("abc123"), 5)
            .expect("marker present");
        assert!(parsed.matches);
        assert_eq!(parsed.actual, "abc123");

        // Download failure before the marker: unknown.
        assert!(runner
            .parse_checksum_result("curl: (22) 404", 22, "https://x", Some("abc"), 1)
            .is_none());
    }

    #[test]
    fn test_retry_policy_retries_network_failures() {
        let result = TestResult::new("network")
            .failed(6, "curl: (6) Could not resolve host: raw.githubusercontent.com");
        assert!(InstallerTestRunner::should_retry_result(&result));
    }

    #[test]
    fn test_retry_policy_does_not_retry_checksum_mismatch() {
        let result = TestResult::new("checksum")
            .failed(99, "CHECKSUM_MISMATCH: expected=abc actual=def")
            .with_checksum_result(ChecksumResult {
                matches: false,
                expected: "abc".to_string(),
                actual: "def".to_string(),
                url: "https://example.com/install.sh".to_string(),
                download_ms: 10,
                size_bytes: 0,
            });
        assert!(!InstallerTestRunner::should_retry_result(&result));
    }

    #[test]
    fn test_retry_policy_does_not_retry_timeout() {
        let result = TestResult::new("timeout").timed_out();
        assert!(!InstallerTestRunner::should_retry_result(&result));
    }

    #[test]
    fn test_finalize_failure_classifies_from_stdout_tail() {
        let mut result = TestResult::new("srps").failed(1, "");
        result.stdout = "banner\nDon't run this script as root. Run as a regular user.\n".into();
        finalize_failure(&mut result, None);
        assert_eq!(result.error.as_ref().unwrap().category, "permission");
        // Idempotent and never applied to passes
        let before = result.error.clone().unwrap().category;
        finalize_failure(&mut result, Some("ignored"));
        assert_eq!(result.error.unwrap().category, before);
        let mut ok = TestResult::new("ok").passed();
        finalize_failure(&mut ok, None);
        assert!(ok.error.is_none());
    }

    #[tokio::test]
    async fn test_runner_local_with_simple_command() {
        let config =
            RunnerConfig { dry_run: false, backend: ExecutionBackend::Local, ..Default::default() };
        let runner = InstallerTestRunner::new(config);

        let test = InstallerTest::new("test-echo", "https://example.com/nonexistent.sh")
            .with_timeout(std::time::Duration::from_secs(10));

        let result = runner.run_test(&test).await;
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.duration_ms > 0 || result.status == TestStatus::TimedOut);
        if !result.success {
            assert!(result.error.is_some(), "failures must be classified");
        }
    }
}
