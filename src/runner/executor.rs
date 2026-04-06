//! Installer execution logic with Docker and local backends
//!
//! This module implements the core test runner that executes installer scripts
//! either inside Docker containers (default) or in isolated local temp directories.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::container::{ContainerConfig, ContainerGuard, ContainerManager, PullPolicy};
use super::installer::{ChecksumResult, InstallerTest, TestResult, TestStatus};

/// Execution backend selection
#[derive(Debug, Clone)]
pub enum ExecutionBackend {
    /// Run installers inside Docker containers (default, recommended)
    Docker {
        container_config: ContainerConfig,
        pull_policy: PullPolicy,
    },
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
    /// Path to curl binary (used in both backends)
    pub curl_path: String,
    /// Path to bash binary (used in both backends)
    pub bash_path: String,
    /// Additional environment variables to set
    pub extra_env: Vec<(String, String)>,
    /// Execution backend
    pub backend: ExecutionBackend,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(300),
            dry_run: false,
            curl_path: "curl".to_string(),
            bash_path: "bash".to_string(),
            extra_env: Vec::new(),
            backend: ExecutionBackend::Docker {
                container_config: ContainerConfig::default(),
                pull_policy: PullPolicy::IfNotPresent,
            },
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
        let dry_run_flag = if self.config.dry_run { " --dry-run" } else { "" };

        match expected_sha256 {
            Some(expected) => {
                // Download → verify → execute script
                let script_path = format!("/tmp/installer_{}.sh", installer_name);
                format!(
                    r#"set -e
{curl} -fsSL '{url}' -o '{path}'
ACTUAL=$(sha256sum '{path}' | cut -d' ' -f1)
if [ "$ACTUAL" != "{expected}" ]; then
  echo "CHECKSUM_MISMATCH: expected={expected} actual=$ACTUAL url={url}" >&2
  exit 99
fi
{bash} '{path}'{flags}"#,
                    curl = self.config.curl_path,
                    url = url,
                    path = script_path,
                    expected = expected,
                    bash = self.config.bash_path,
                    flags = dry_run_flag,
                )
            }
            None => {
                // No checksum — use curl|bash directly
                format!(
                    "{} -fsSL '{}' | {} -s --{}",
                    self.config.curl_path, url, self.config.bash_path, dry_run_flag
                )
            }
        }
    }

    /// Parse checksum result from stderr output (looks for CHECKSUM_MISMATCH marker)
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
            // Extract actual hash from the error message
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
        } else {
            // Checksum passed (or wasn't checked due to download failure)
            Some(ChecksumResult {
                matches: true,
                expected: expected.to_string(),
                actual: expected.to_string(),
                url: url.to_string(),
                download_ms,
                size_bytes: 0,
            })
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

    /// Run an installer test using the configured backend
    pub async fn run_test(&self, test: &InstallerTest) -> Result<TestResult> {
        match &self.config.backend {
            ExecutionBackend::Docker { container_config, pull_policy } => {
                self.run_test_docker(test, container_config, pull_policy).await
            }
            ExecutionBackend::Local => self.run_test_local(test).await,
        }
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
        let manager =
            ContainerManager::new(config).with_pull_policy(pull_policy.clone());

        let container_id = manager
            .create_container(&test.name)
            .await
            .context("Failed to create Docker container")?;

        // Set up guard for cleanup on early return/panic
        let mut guard = ContainerGuard::new(container_id.clone(), manager.docker_arc());
        result = result.with_container_id(&container_id);

        // Ensure curl is available in the container (ubuntu:22.04 doesn't include it)
        debug!(container_id = %container_id, "Installing curl in container");
        if let Err(e) = manager
            .exec_in_container(
                &container_id,
                &["bash", "-c", "apt-get update -qq && apt-get install -y -qq curl ca-certificates >/dev/null 2>&1"],
            )
            .await
        {
            warn!(container_id = %container_id, error = %e, "Failed to install curl in container");
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
                result.stderr = format!("Test timed out after {:?}", test_timeout);
                result.duration = test_timeout;
                result.duration_ms = test_timeout.as_millis() as u64;
            }
        }

        // Always clean up the container
        guard.cleanup().await;
        result.finished_at = chrono::Utc::now();
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
            let dl_output = Command::new(&self.config.curl_path)
                .args(["-fsSL", &test.url, "-o"])
                .arg(&script_file)
                .output()
                .await
                .context("Failed to download installer script")?;

            let download_ms = download_start.elapsed().as_millis() as u64;

            if !dl_output.status.success() {
                let stderr = String::from_utf8_lossy(&dl_output.stderr).to_string();
                result.stderr = format!("Download failed: {}", stderr);
                result.status = TestStatus::Failed;
                result.success = false;
                result.finished_at = chrono::Utc::now();
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
        // execute the local file directly instead of re-downloading.
        let curl_bash_script = if test.expected_sha256.is_some() {
            let script_file = temp_path.join(format!("installer_{}.sh", test.name));
            let dry_run_flag = if self.config.dry_run { " --dry-run" } else { "" };
            format!(
                "{} '{}'{}",
                self.config.bash_path,
                script_file.display(),
                dry_run_flag
            )
        } else {
            self.build_verified_install_script(&test.url, &test.name, None)
        };

        debug!(script = %curl_bash_script, "Executing installer script locally");

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
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
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
                result.stderr = format!("Test timed out after {:?}", test_timeout);
                result.duration = test_timeout;
                result.duration_ms = test_timeout.as_millis() as u64;
            }
        }

        debug!(path = ?temp_path, "Cleaning up temp directory");
        result.finished_at = chrono::Utc::now();
        Ok(result)
    }

    /// Run a test with retries (each retry creates a fresh container in Docker mode)
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

            let stderr_copy = result.stderr.clone();
            result.add_retry(&stderr_copy, wait_ms);
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
        assert!(matches!(config.backend, ExecutionBackend::Docker { .. }));
    }

    #[test]
    fn test_runner_config_local_backend() {
        let config = RunnerConfig {
            backend: ExecutionBackend::Local,
            ..Default::default()
        };
        assert!(matches!(config.backend, ExecutionBackend::Local));
    }

    #[test]
    fn test_backoff_calculation() {
        let config = RunnerConfig {
            backend: ExecutionBackend::Local,
            ..Default::default()
        };
        let runner = InstallerTestRunner::new(config);

        let backoff1 = runner.calculate_backoff(1);
        assert!(backoff1 >= 2000 && backoff1 <= 3000);

        let backoff2 = runner.calculate_backoff(2);
        assert!(backoff2 >= 4000 && backoff2 <= 6000);
    }

    #[test]
    fn test_dry_run_false_does_not_append_flag() {
        // Regression (br-74o.13): When dry_run is false, the curl|bash command
        // must NOT contain --dry-run.
        let config = RunnerConfig {
            dry_run: false,
            backend: ExecutionBackend::Local,
            ..Default::default()
        };
        let runner = InstallerTestRunner::new(config);
        let cmd = runner.build_verified_install_script("https://example.com/install.sh", "test", None);
        assert!(
            !cmd.contains("--dry-run"),
            "Command must not contain --dry-run when dry_run=false"
        );
    }

    #[test]
    fn test_dry_run_true_appends_flag() {
        let config = RunnerConfig {
            dry_run: true,
            backend: ExecutionBackend::Local,
            ..Default::default()
        };
        let runner = InstallerTestRunner::new(config);
        let cmd = runner.build_verified_install_script("https://example.com/install.sh", "test", None);
        assert!(
            cmd.contains("--dry-run"),
            "Command must contain --dry-run when dry_run=true"
        );
    }

    #[test]
    fn test_build_install_command_format() {
        let config = RunnerConfig {
            dry_run: false,
            backend: ExecutionBackend::Local,
            ..Default::default()
        };
        let runner = InstallerTestRunner::new(config);
        let cmd = runner.build_verified_install_script("https://example.com/install.sh", "test", None);
        assert!(cmd.contains("curl -fsSL"));
        assert!(cmd.contains("https://example.com/install.sh"));
        assert!(cmd.contains("| bash -s --"));
    }

    #[test]
    fn test_build_verified_script_with_checksum() {
        let config = RunnerConfig {
            dry_run: false,
            backend: ExecutionBackend::Local,
            ..Default::default()
        };
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
        assert!(cmd.contains("exit 99"));
        // Should NOT contain pipe to bash
        assert!(!cmd.contains("| bash"));
    }

    #[test]
    fn test_build_verified_script_without_checksum() {
        let config = RunnerConfig {
            dry_run: false,
            backend: ExecutionBackend::Local,
            ..Default::default()
        };
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

    #[tokio::test]
    async fn test_runner_local_with_simple_command() {
        let config = RunnerConfig {
            dry_run: false,
            backend: ExecutionBackend::Local,
            ..Default::default()
        };
        let runner = InstallerTestRunner::new(config);

        let test = InstallerTest::new("test-echo", "https://example.com/nonexistent.sh")
            .with_timeout(std::time::Duration::from_secs(10));

        let result = runner.run_test(&test).await;
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.duration_ms > 0 || result.status == TestStatus::TimedOut);
    }
}
