//! Actual installer execution logic
//!
//! This module implements the core test runner that executes installer scripts
//! in isolated temp directories and captures results.

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::installer::{InstallerTest, TestResult, TestStatus};

/// Configuration for the installer test runner
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// Default timeout for tests
    pub default_timeout: Duration,
    /// Whether to run in dry-run mode (--dry-run flag)
    pub dry_run: bool,
    /// Path to curl binary
    pub curl_path: String,
    /// Path to bash binary
    pub bash_path: String,
    /// Additional environment variables to set
    pub extra_env: Vec<(String, String)>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(300),
            dry_run: false,
            curl_path: "curl".to_string(),
            bash_path: "bash".to_string(),
            extra_env: Vec::new(),
        }
    }
}

/// Executes installer tests in isolated environments
pub struct InstallerTestRunner {
    config: RunnerConfig,
}

impl InstallerTestRunner {
    pub fn new(config: RunnerConfig) -> Self {
        Self { config }
    }

    /// Run an installer test and capture results
    ///
    /// This creates a temp directory, downloads and runs the installer script,
    /// captures stdout/stderr, and cleans up afterward.
    pub async fn run_test(&self, test: &InstallerTest) -> Result<TestResult> {
        let mut result = TestResult::new(&test.name);
        let start_time = Instant::now();

        info!(installer = %test.name, url = %test.url, "Starting installer test");

        // Create isolated temp directory
        let temp_dir = TempDir::new().context("Failed to create temp directory")?;
        let temp_path = temp_dir.path().to_path_buf();
        debug!(path = ?temp_path, "Created temp directory");

        // Determine timeout
        let test_timeout = if test.timeout.as_secs() > 0 {
            test.timeout
        } else {
            self.config.default_timeout
        };

        // Build the command: curl -fsSL $URL | bash -s -- [--dry-run]
        let curl_bash_script = if self.config.dry_run {
            format!(
                "{} -fsSL '{}' | {} -s -- --dry-run",
                self.config.curl_path, test.url, self.config.bash_path
            )
        } else {
            format!(
                "{} -fsSL '{}' | {} -s --",
                self.config.curl_path, test.url, self.config.bash_path
            )
        };

        debug!(script = %curl_bash_script, "Executing installer script");

        // Create the command
        let mut cmd = Command::new(&self.config.bash_path);
        cmd.arg("-c")
            .arg(&curl_bash_script)
            .current_dir(&temp_path)
            .env("HOME", &temp_path)
            .env("TMPDIR", &temp_path)
            .env("XDG_CONFIG_HOME", temp_path.join(".config"))
            .env("XDG_DATA_HOME", temp_path.join(".local/share"))
            .env("XDG_CACHE_HOME", temp_path.join(".cache"))
            // Restrict PATH to essential system directories
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
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

            // Read both streams concurrently
            let (stdout_result, stderr_result) = tokio::join!(
                stdout_handle.read_to_end(&mut stdout_buf),
                stderr_handle.read_to_end(&mut stderr_buf)
            );

            stdout_result.context("Failed to read stdout")?;
            stderr_result.context("Failed to read stderr")?;

            // Wait for process to complete
            let status = child.wait().await.context("Failed to wait for process")?;

            Ok::<_, anyhow::Error>((status, stdout_buf, stderr_buf))
        })
        .await;

        // Handle the result
        match execution_result {
            Ok(Ok((status, stdout_buf, stderr_buf))) => {
                let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
                let stderr = String::from_utf8_lossy(&stderr_buf).to_string();
                let exit_code = status.code().unwrap_or(-1);
                let elapsed = start_time.elapsed();

                result.stdout = stdout;
                result.stderr = stderr.clone();
                result.exit_code = Some(exit_code);
                result.duration = elapsed;
                result.duration_ms = elapsed.as_millis() as u64;

                if status.success() {
                    info!(
                        installer = %test.name,
                        duration_ms = elapsed.as_millis(),
                        "Installer test passed"
                    );
                    result.status = TestStatus::Passed;
                    result.success = true;
                } else {
                    warn!(
                        installer = %test.name,
                        exit_code = exit_code,
                        duration_ms = elapsed.as_millis(),
                        "Installer test failed"
                    );
                    result.status = TestStatus::Failed;
                    result.success = false;
                }
            }
            Ok(Err(e)) => {
                warn!(installer = %test.name, error = %e, "Installer execution error");
                result.stderr = format!("Execution error: {}", e);
                result.status = TestStatus::Failed;
                result.success = false;
            }
            Err(_) => {
                // Timeout occurred - kill the process
                warn!(
                    installer = %test.name,
                    timeout_seconds = test_timeout.as_secs(),
                    "Installer test timed out"
                );

                // Try to kill the process
                if let Err(e) = child.kill().await {
                    debug!(error = %e, "Failed to kill timed-out process");
                }

                result.status = TestStatus::TimedOut;
                result.success = false;
                result.stderr = format!("Test timed out after {:?}", test_timeout);
                result.duration = test_timeout;
                result.duration_ms = test_timeout.as_millis() as u64;
            }
        }

        // Temp directory is automatically cleaned up when TempDir is dropped
        debug!(path = ?temp_path, "Cleaning up temp directory");

        result.finished_at = chrono::Utc::now();
        Ok(result)
    }

    /// Run a test with retries
    pub async fn run_test_with_retry(&self, test: &InstallerTest) -> Result<TestResult> {
        let mut result = self.run_test(test).await?;
        let mut attempts = 1;

        while !result.success && attempts < test.retry_count {
            let wait_ms = self.calculate_backoff(attempts);
            info!(
                installer = %test.name,
                attempt = attempts + 1,
                wait_ms = wait_ms,
                "Retrying failed test"
            );

            result.add_retry(&result.stderr.clone(), wait_ms);
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;

            result = self.run_test(test).await?;
            attempts += 1;
        }

        result.max_attempts = test.retry_count;
        Ok(result)
    }

    /// Calculate exponential backoff with jitter
    fn calculate_backoff(&self, attempt: u32) -> u64 {
        let base_ms: u64 = 1000;
        let max_ms: u64 = 30000;
        let exponential = base_ms * 2u64.pow(attempt.min(10));
        let jitter = rand::random::<u64>() % (exponential / 4 + 1);
        (exponential + jitter).min(max_ms)
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
        assert_eq!(config.curl_path, "curl");
        assert_eq!(config.bash_path, "bash");
    }

    #[test]
    fn test_backoff_calculation() {
        let runner = InstallerTestRunner::new(RunnerConfig::default());

        // First retry should be around 2 seconds (with some jitter)
        let backoff1 = runner.calculate_backoff(1);
        assert!(backoff1 >= 2000 && backoff1 <= 3000);

        // Second retry should be around 4 seconds
        let backoff2 = runner.calculate_backoff(2);
        assert!(backoff2 >= 4000 && backoff2 <= 6000);
    }

    #[tokio::test]
    async fn test_runner_with_simple_command() {
        let config = RunnerConfig {
            dry_run: false,
            ..Default::default()
        };
        let runner = InstallerTestRunner::new(config);

        // Test with a URL that should fail (example.com returns HTML, not a script)
        let test = InstallerTest::new("test-echo", "https://example.com/nonexistent.sh")
            .with_timeout(std::time::Duration::from_secs(10));

        // This exercises the execution path
        let result = runner.run_test(&test).await;
        assert!(result.is_ok());
        let result = result.unwrap();
        // Note: The test may succeed or fail depending on network conditions
        // and what example.com returns. The important thing is that the
        // runner completes without panicking.
        assert!(result.duration_ms > 0 || result.status == TestStatus::TimedOut);
    }
}
