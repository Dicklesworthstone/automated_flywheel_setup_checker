//! Installer execution logic with Docker and local backends
//!
//! This module implements the core test runner that executes installer scripts
//! either inside Docker containers (default) or in isolated local temp directories.
//!
//! Contract (shared by both backends):
//! - installers run the way ACFS runs them: the script is downloaded to a staged file, its
//!   SHA-256 is verified, and it is executed as `<interpreter> <file> <args...>` with the
//!   installer's environment (see [`InstallerTest`] and the ACFS profile table);
//! - every non-passing result carries an [`ErrorClassification`] (see [`finalize_failure`]);
//! - retries accumulate an [`AttemptRecord`] per attempt instead of replacing the result;
//! - checksum verification runs before execution and a mismatch is never retried;
//! - optional post-install checks (`expect_binary`, `verify_cmd`, `version_cmd`) run in the same
//!   environment after a passing install;
//! - the run's [`CancellationToken`] stops in-flight installers (container stopped, local child
//!   killed) and yields `Cancelled` results instead of leaking work.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::parser::{
    classify_error, ErrorClassification, CANCELLED_MARKER, POST_INSTALL_MARKER, TIMEOUT_MARKER,
};
use crate::reporting::redact;

use super::container::{ContainerConfig, ContainerGuard, ContainerManager, PullPolicy};
use super::installer::{
    bound_capture, tail, AttemptRecord, ChecksumResult, ChecksumState, InstallerTest, RetryInfo,
    TestResult, TestStatus, DEFAULT_MAX_CAPTURE_BYTES,
};
use super::retry::RetryConfig;

const CURL_BIN: &str = "curl";

/// Bytes of stdout appended to the classification input.
const CLASSIFY_STDOUT_TAIL_BYTES: usize = 4096;
/// Bytes of stderr kept per attempt record.
const ATTEMPT_STDERR_TAIL_BYTES: usize = 1024;
/// Additive jitter fraction applied to retry backoff.
const RETRY_JITTER_FRACTION: f64 = 0.25;
/// Timeout for each post-install check command.
const POST_INSTALL_TIMEOUT: Duration = Duration::from_secs(60);

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
    /// Run-wide cancellation (signals, fail-fast, deadline)
    pub cancel: CancellationToken,
    /// Cap on captured stdout/stderr bytes per attempt (0 = unbounded)
    pub max_capture_bytes: usize,
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
            cancel: CancellationToken::new(),
            max_capture_bytes: DEFAULT_MAX_CAPTURE_BYTES,
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

/// Mark a result as cancelled by the run token.
fn mark_cancelled(result: &mut TestResult, start: Instant, reason: &str) {
    result.status = TestStatus::Cancelled;
    result.success = false;
    result.stderr = format!("{CANCELLED_MARKER}: {reason}");
    result.duration = start.elapsed();
    result.duration_ms = result.duration.as_millis() as u64;
    result.last_attempt_ms = result.duration_ms;
    result.finished_at = chrono::Utc::now();
}

/// Single-quote a string for POSIX shells.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Where post-install checks execute.
enum Exec<'a> {
    Docker { manager: &'a ContainerManager, container_id: &'a str, cancel: &'a CancellationToken },
    Local { env: Vec<(String, String)>, cwd: PathBuf },
}

impl Exec<'_> {
    async fn run(&self, cmd: &str) -> Result<(i32, String, String)> {
        match self {
            Exec::Docker { manager, container_id, cancel } => {
                let out = timeout(
                    POST_INSTALL_TIMEOUT,
                    manager.exec_in_container_cancellable(
                        container_id,
                        &["bash", "-lc", cmd],
                        cancel,
                    ),
                )
                .await
                .context("post-install check timed out")??;
                Ok(out)
            }
            Exec::Local { env, cwd } => {
                let mut c = Command::new("bash");
                c.arg("-c").arg(cmd).current_dir(cwd).env_clear();
                for (k, v) in env {
                    c.env(k, v);
                }
                let out = timeout(POST_INSTALL_TIMEOUT, c.output())
                    .await
                    .context("post-install check timed out")??;
                Ok((
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stdout).to_string(),
                    String::from_utf8_lossy(&out.stderr).to_string(),
                ))
            }
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

    /// The run-wide cancellation token.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.config.cancel
    }

    /// Effective installer arguments (`--dry-run` appended in dry-run mode).
    fn installer_args(&self, test: &InstallerTest) -> Vec<String> {
        let mut args = test.args.clone();
        if self.config.dry_run {
            args.push("--dry-run".to_string());
        }
        args
    }

    /// `<interpreter> '<script>' 'arg'...` as ACFS runs it.
    pub fn installer_command(&self, test: &InstallerTest, script_path: &str) -> String {
        let mut cmd = format!("{} {}", test.interpreter, shell_quote(script_path));
        for a in self.installer_args(test) {
            cmd.push(' ');
            cmd.push_str(&shell_quote(&a));
        }
        cmd
    }

    /// Build the shell script that downloads, verifies, stages, and executes the installer.
    ///
    /// With a pinned hash the script downloads to a staged file, compares its SHA-256, prints
    /// `CHECKSUM_OK <hash>` (or `CHECKSUM_MISMATCH …` and exits 99), then runs the installer via
    /// [`installer_command`](Self::installer_command). Without a hash it still stages the file so
    /// the interpreter/argument shape is identical.
    pub fn build_verified_install_script(&self, test: &InstallerTest) -> String {
        let script_path = format!("/tmp/installer_{}.sh", test.name);
        let run = self.installer_command(test, &script_path);
        let url = &test.url;
        match test.expected_sha256.as_deref() {
            Some(expected) => format!(
                r#"set -e
rm -f '{path}'
{curl} -fsSL '{url}' -o '{path}'
ACTUAL=$( (command -v sha256sum >/dev/null 2>&1 && sha256sum '{path}' || shasum -a 256 '{path}') | cut -d' ' -f1)
if [ "$ACTUAL" != "{expected}" ]; then
  echo "CHECKSUM_MISMATCH: expected={expected} actual=$ACTUAL url={url}" >&2
  exit 99
fi
echo "CHECKSUM_OK $ACTUAL" >&2
chmod 0444 '{path}'
set +e
{run}"#,
                curl = CURL_BIN,
                url = url,
                path = script_path,
                expected = expected,
                run = run,
            ),
            None => format!(
                "set -e\nrm -f '{path}'\n{curl} -fsSL '{url}' -o '{path}'\nchmod 0444 '{path}'\nset +e\n{run}",
                curl = CURL_BIN,
                url = url,
                path = script_path,
                run = run,
            ),
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

    /// Run post-install checks after a passing install. Mutates the result on failure.
    async fn post_install(&self, test: &InstallerTest, result: &mut TestResult, exec: &Exec<'_>) {
        if let Some(bin) = &test.expect_binary {
            let cmd = format!("command -v {}", shell_quote(bin));
            match exec.run(&cmd).await {
                Ok((0, out, _)) => {
                    debug!(installer = %test.name, binary = %bin, path = %out.trim(), "expect_binary found");
                }
                Ok((code, _, err)) => {
                    warn!(installer = %test.name, binary = %bin, "expect_binary not found on PATH");
                    result.status = TestStatus::Failed;
                    result.success = false;
                    result.exit_code = Some(code);
                    result.stderr.push_str(&format!(
                        "\n{POST_INSTALL_MARKER}: expected binary '{bin}' not found on PATH\n{err}"
                    ));
                    return;
                }
                Err(e) => {
                    result.status = TestStatus::Failed;
                    result.success = false;
                    result.stderr.push_str(&format!(
                        "\n{POST_INSTALL_MARKER}: expect_binary check error: {e}"
                    ));
                    return;
                }
            }
        }
        if let Some(cmd) = &test.verify_cmd {
            match exec.run(cmd).await {
                Ok((0, _, _)) => {
                    debug!(installer = %test.name, "verify_cmd passed");
                }
                Ok((code, out, err)) => {
                    warn!(installer = %test.name, exit_code = code, "verify_cmd failed");
                    result.status = TestStatus::Failed;
                    result.success = false;
                    result.exit_code = Some(code);
                    result.stderr.push_str(&format!(
                        "\n{POST_INSTALL_MARKER}: verify_cmd exited {code}\n{}\n{}",
                        redact(&tail(&out, 1024)),
                        redact(&tail(&err, 1024))
                    ));
                    return;
                }
                Err(e) => {
                    result.status = TestStatus::Failed;
                    result.success = false;
                    result
                        .stderr
                        .push_str(&format!("\n{POST_INSTALL_MARKER}: verify_cmd error: {e}"));
                    return;
                }
            }
        }
        if let Some(cmd) = &test.version_cmd {
            match exec.run(cmd).await {
                Ok((0, out, _)) => {
                    let first =
                        out.lines().find(|l| !l.trim().is_empty()).map(|l| redact(l.trim()));
                    if let Some(v) = &first {
                        info!(installer = %test.name, version = %v, "Installed version");
                    }
                    result.installed_version = first;
                }
                Ok((code, _, _)) => {
                    debug!(installer = %test.name, exit_code = code, "version_cmd failed (ignored)");
                }
                Err(e) => {
                    debug!(installer = %test.name, error = %e, "version_cmd error (ignored)");
                }
            }
        }
    }

    /// Run an installer test using the configured backend (single attempt)
    pub async fn run_test(&self, test: &InstallerTest) -> Result<TestResult> {
        if self.config.cancel.is_cancelled() {
            let mut result = TestResult::new(&test.name);
            mark_cancelled(&mut result, Instant::now(), "run cancelled before start");
            finalize_failure(&mut result, None);
            return Ok(result);
        }
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
        let cancel = &self.config.cancel;

        info!(
            installer = %test.name,
            url = %test.url,
            backend = "docker",
            "Starting installer test"
        );

        // Build container config with test-specific environment and overrides
        let mut config = container_config.clone();
        for (key, value) in &test.environment {
            config.environment.push((key.clone(), value.clone()));
        }
        for (key, value) in &self.config.extra_env {
            config.environment.push((key.clone(), value.clone()));
        }
        if let Some(mem) = test.memory_limit {
            config.memory_limit = Some(mem);
        }
        if let Some(net) = &test.network {
            config.network_mode = Some(net.clone());
        }
        if let Some(root) = test.run_as_root {
            config.run_as_root = root;
        }

        // Create container manager and container
        let manager = ContainerManager::try_new(config)?.with_pull_policy(pull_policy.clone());

        let container_id = manager
            .create_container(&test.name)
            .await
            .context("Failed to create Docker container")?;

        // Set up guard for cleanup on early return/panic
        let mut guard = ContainerGuard::new(container_id.clone(), manager.docker_arc());
        result = result.with_container_id(&container_id);

        // Build the verified install script (download → checksum → execute)
        let install_script = self.build_verified_install_script(test);
        debug!(
            container_id = %container_id,
            has_checksum = test.expected_sha256.is_some(),
            command = %self.installer_command(test, &format!("/tmp/installer_{}.sh", test.name)),
            "Executing installer in container"
        );

        // Execute with timeout and cancellation, sampling resource usage meanwhile.
        let telemetry_stop = CancellationToken::new();
        let telemetry = manager.spawn_telemetry(container_id.clone(), telemetry_stop.clone());
        let exec_result = timeout(
            test_timeout,
            manager.exec_in_container_cancellable(
                &container_id,
                &["bash", "-c", &install_script],
                cancel,
            ),
        )
        .await;
        telemetry_stop.cancel();
        if let Ok(t) = telemetry.await {
            if t.samples > 0 {
                result.telemetry = Some(t);
            }
        }

        match exec_result {
            Ok(Ok((exit_code, stdout, stderr))) => {
                let elapsed = start_time.elapsed();
                let stdout =
                    redact(&bound_capture(stdout.as_bytes(), self.config.max_capture_bytes));
                let stderr =
                    redact(&bound_capture(stderr.as_bytes(), self.config.max_capture_bytes));
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
                } else if test.expected_sha256.is_some() {
                    // Download failed before the compare could run.
                    result = result.with_checksum_state(ChecksumState::Unknown);
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
                    let exec =
                        Exec::Docker { manager: &manager, container_id: &container_id, cancel };
                    self.post_install(test, &mut result, &exec).await;
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
            Ok(Err(e)) if cancel.is_cancelled() => {
                warn!(
                    installer = %test.name,
                    container_id = %container_id,
                    "Installer test cancelled"
                );
                mark_cancelled(&mut result, start_time, &e.to_string());
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

    /// Environment for local execution: an isolated HOME with the usual install locations on PATH.
    fn local_env(&self, test: &InstallerTest, temp_path: &Path) -> Vec<(String, String)> {
        let home = temp_path.to_string_lossy().to_string();
        let mut env: Vec<(String, String)> = vec![
            ("HOME".into(), home.clone()),
            ("TMPDIR".into(), home.clone()),
            ("XDG_CONFIG_HOME".into(), temp_path.join(".config").to_string_lossy().to_string()),
            ("XDG_DATA_HOME".into(), temp_path.join(".local/share").to_string_lossy().to_string()),
            ("XDG_CACHE_HOME".into(), temp_path.join(".cache").to_string_lossy().to_string()),
            (
                "PATH".into(),
                format!(
                    "{home}/.local/bin:{home}/.cargo/bin:{home}/.bun/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                ),
            ),
            ("DEBIAN_FRONTEND".into(), "noninteractive".into()),
            ("NONINTERACTIVE".into(), "1".into()),
            ("CI".into(), "true".into()),
            ("RUSTUP_INIT_SKIP_PATH_CHECK".into(), "yes".into()),
        ];
        for (key, value) in &test.environment {
            env.push((key.clone(), value.clone()));
        }
        for (key, value) in &self.config.extra_env {
            env.push((key.clone(), value.clone()));
        }
        env
    }

    /// Run an installer test locally in an isolated temp directory (fallback)
    async fn run_test_local(&self, test: &InstallerTest) -> Result<TestResult> {
        let mut result = TestResult::new(&test.name);
        let start_time = Instant::now();
        let cancel = &self.config.cancel;

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

        // Download to a staged file (always), verify when a hash is pinned.
        let script_file = temp_path.join(format!("installer_{}.sh", test.name));
        let download_start = Instant::now();
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
            if test.expected_sha256.is_some() {
                result.checksum_state = ChecksumState::Unknown;
            }
            result.finished_at = chrono::Utc::now();
            result.duration = start_time.elapsed();
            result.duration_ms = result.duration.as_millis() as u64;
            result.last_attempt_ms = result.duration_ms;
            return Ok(result);
        }

        if let Some(expected_sha256) = &test.expected_sha256 {
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

        let _ = std::fs::set_permissions(&script_file, {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::Permissions::from_mode(0o444)
            }
            #[cfg(not(unix))]
            {
                std::fs::metadata(&script_file)?.permissions()
            }
        });

        let command_line = self.installer_command(test, &script_file.to_string_lossy());
        debug!(command = %command_line, "Executing installer script locally");

        let env = self.local_env(test, &temp_path);
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(&command_line)
            .current_dir(&temp_path)
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &env {
            cmd.env(key, value);
        }

        // Spawn the process
        let mut child = cmd.spawn().context("Failed to spawn installer process")?;

        // Get stdout/stderr handles
        let mut stdout_handle = child.stdout.take().expect("stdout was piped");
        let mut stderr_handle = child.stderr.take().expect("stderr was piped");

        // Read outputs with timeout and cancellation
        let execution = async {
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
        };

        enum Outcome {
            Done(Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)>),
            TimedOut,
            Cancelled,
        }

        let outcome = tokio::select! {
            _ = cancel.cancelled() => Outcome::Cancelled,
            r = timeout(test_timeout, execution) => match r {
                Ok(inner) => Outcome::Done(inner),
                Err(_) => Outcome::TimedOut,
            },
        };

        match outcome {
            Outcome::Done(Ok((status, stdout_buf, stderr_buf))) => {
                let stdout = redact(&bound_capture(&stdout_buf, self.config.max_capture_bytes));
                let stderr = redact(&bound_capture(&stderr_buf, self.config.max_capture_bytes));
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
                    let exec = Exec::Local { env, cwd: temp_path.clone() };
                    self.post_install(test, &mut result, &exec).await;
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
            Outcome::Done(Err(e)) => {
                warn!(installer = %test.name, error = %e, "Installer execution error (local)");
                result.stderr = format!("Execution error: {}", e);
                result.status = TestStatus::Failed;
                result.success = false;
                result.duration = start_time.elapsed();
                result.duration_ms = result.duration.as_millis() as u64;
            }
            Outcome::TimedOut => {
                warn!(
                    installer = %test.name,
                    timeout_seconds = test_timeout.as_secs(),
                    "Installer test timed out (local)"
                );
                // The child is killed on drop (kill_on_drop); nothing else holds it.
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
            Outcome::Cancelled => {
                warn!(installer = %test.name, "Installer test cancelled (local)");
                mark_cancelled(&mut result, start_time, "cancelled while running installer");
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

            let retry = attempt_index < max_attempts
                && !self.config.cancel.is_cancelled()
                && Self::should_retry_result(&result);
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
            tokio::select! {
                _ = self.config.cancel.cancelled() => {
                    // Cancelled during backoff: report the last attempt as cancelled.
                    mark_cancelled(&mut result, overall_start, "cancelled during retry backoff");
                    finalize_failure(&mut result, None);
                    result.attempt = attempt_index;
                    result.max_attempts = max_attempts;
                    result.attempts = attempts;
                    result.retries = retries;
                    result.started_at = started_at;
                    return Ok(result);
                }
                _ = tokio::time::sleep(wait) => {}
            }
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

        if matches!(
            result.status,
            TestStatus::TimedOut | TestStatus::Cancelled | TestStatus::Skipped
        ) {
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

    fn local_runner(dry_run: bool) -> InstallerTestRunner {
        InstallerTestRunner::new(RunnerConfig {
            dry_run,
            backend: ExecutionBackend::Local,
            ..Default::default()
        })
    }

    #[test]
    fn test_runner_config_default() {
        let config = RunnerConfig::default();
        assert_eq!(config.default_timeout, Duration::from_secs(300));
        assert!(!config.dry_run);
        assert!(matches!(config.backend, ExecutionBackend::Docker { .. }));
        assert_eq!(config.retry.max_attempts, 4);
        assert!(!config.cancel.is_cancelled());
    }

    #[test]
    fn test_runner_config_local_backend() {
        let config = RunnerConfig { backend: ExecutionBackend::Local, ..Default::default() };
        assert!(matches!(config.backend, ExecutionBackend::Local));
    }

    #[test]
    fn test_backoff_calculation() {
        let runner = local_runner(false);
        let backoff1 = runner.config.retry.delay_with_jitter(1, RETRY_JITTER_FRACTION);
        assert!((2000..=2500).contains(&(backoff1.as_millis() as u64)));
        let backoff2 = runner.config.retry.delay_with_jitter(2, RETRY_JITTER_FRACTION);
        assert!((4000..=5000).contains(&(backoff2.as_millis() as u64)));
    }

    #[test]
    fn test_dry_run_false_does_not_append_flag() {
        // Regression (br-74o.13): when dry_run is false the installer must NOT receive --dry-run.
        let runner = local_runner(false);
        let test = InstallerTest::new("test", "https://example.com/install.sh");
        let cmd = runner.build_verified_install_script(&test);
        assert!(
            !cmd.contains("--dry-run"),
            "Command must not contain --dry-run when dry_run=false"
        );
    }

    #[test]
    fn test_dry_run_true_appends_flag() {
        let runner = local_runner(true);
        let test = InstallerTest::new("test", "https://example.com/install.sh");
        let cmd = runner.build_verified_install_script(&test);
        assert!(cmd.contains("'--dry-run'"), "Command must contain --dry-run when dry_run=true");
    }

    #[test]
    fn test_staged_execution_without_checksum() {
        let runner = local_runner(false);
        let test = InstallerTest::new("test", "https://example.com/install.sh");
        let cmd = runner.build_verified_install_script(&test);
        assert!(
            cmd.contains("curl -fsSL 'https://example.com/install.sh' -o '/tmp/installer_test.sh'")
        );
        assert!(cmd.contains("bash '/tmp/installer_test.sh'"));
        assert!(!cmd.contains("sha256sum"));
        assert!(!cmd.contains("| bash"), "no curl|bash piping");
    }

    #[test]
    fn test_build_verified_script_with_checksum() {
        let runner = local_runner(false);
        let test = InstallerTest::new("myinstaller", "https://example.com/install.sh")
            .with_sha256("abc123def456");
        let cmd = runner.build_verified_install_script(&test);
        assert!(cmd.contains("-o '/tmp/installer_myinstaller.sh'"));
        assert!(cmd.contains("sha256sum"));
        assert!(cmd.contains("abc123def456"));
        assert!(cmd.contains("CHECKSUM_MISMATCH"));
        assert!(cmd.contains("CHECKSUM_OK"));
        assert!(cmd.contains("exit 99"));
        assert!(cmd.contains("chmod 0444 '/tmp/installer_myinstaller.sh'"));
        assert!(cmd.ends_with("bash '/tmp/installer_myinstaller.sh'"));
    }

    #[test]
    fn test_interpreter_and_args_follow_the_spec() {
        let runner = local_runner(false);
        let test = InstallerTest::new("ohmyzsh", "https://install.ohmyz.sh/")
            .with_sha256("abc")
            .with_interpreter("sh")
            .with_args(vec!["--unattended".into(), "--keep-zshrc".into()]);
        let cmd = runner.build_verified_install_script(&test);
        assert!(
            cmd.ends_with("sh '/tmp/installer_ohmyzsh.sh' '--unattended' '--keep-zshrc'"),
            "{cmd}"
        );
        let rust = InstallerTest::new("rust", "https://sh.rustup.rs")
            .with_sha256("abc")
            .with_interpreter("sh")
            .with_args(vec!["-y".into()]);
        assert!(runner
            .build_verified_install_script(&rust)
            .ends_with("sh '/tmp/installer_rust.sh' '-y'"));
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_parse_checksum_verified_but_installer_failed() {
        let runner = local_runner(false);
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
        let runner = local_runner(false);
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

    fn file_fixture(dir: &Path, name: &str, body: &str) -> (String, String) {
        let script = dir.join(format!("{name}.sh"));
        std::fs::write(&script, body).unwrap();
        let sha = hex::encode(Sha256::digest(std::fs::read(&script).unwrap()));
        (format!("file://{}", script.display()), sha)
    }

    #[tokio::test]
    async fn test_local_run_passes_args_env_and_interpreter() {
        let runner = local_runner(false);
        let dir = tempfile::tempdir().unwrap();
        let (url, sha) = file_fixture(
            dir.path(),
            "echo",
            "#!/bin/sh\necho \"args=$* MYVAR=$MYVAR\"\nls /proc/$$/exe >/dev/null 2>&1; readlink /proc/$$/exe\nexit 0\n",
        );
        let test = InstallerTest::new("echo", url)
            .with_sha256(sha)
            .with_interpreter("sh")
            .with_args(vec!["--flag".into(), "value with space".into()])
            .with_env("MYVAR", "42")
            .with_timeout(Duration::from_secs(20));
        let result = runner.run_test(&test).await.unwrap();
        assert_eq!(result.status, TestStatus::Passed, "stderr: {}", result.stderr);
        assert!(
            result.stdout.contains("args=--flag value with space MYVAR=42"),
            "{}",
            result.stdout
        );
        assert!(!result.stdout.contains("/bash"), "ran under sh, not bash: {}", result.stdout);
    }

    #[tokio::test]
    async fn test_post_install_checks_local() {
        let runner = local_runner(false);
        let dir = tempfile::tempdir().unwrap();
        let (url, sha) = file_fixture(
            dir.path(),
            "tool",
            "#!/bin/bash\nmkdir -p \"$HOME/.local/bin\"\nprintf '#!/bin/sh\\necho tool 1.2.3\\n' > \"$HOME/.local/bin/mytool\"\nchmod +x \"$HOME/.local/bin/mytool\"\nexit 0\n",
        );
        let base =
            InstallerTest::new("tool", url).with_sha256(sha).with_timeout(Duration::from_secs(20));

        let ok = runner
            .run_test(
                &base.clone().with_expect_binary("mytool").with_version_cmd("mytool --version"),
            )
            .await
            .unwrap();
        assert_eq!(ok.status, TestStatus::Passed, "{}", ok.stderr);
        assert_eq!(ok.installed_version.as_deref(), Some("tool 1.2.3"));

        let missing = runner
            .run_test(&base.clone().with_expect_binary("definitely-missing-xyz"))
            .await
            .unwrap();
        assert_eq!(missing.status, TestStatus::Failed);
        assert_eq!(missing.error.as_ref().unwrap().category, "post_install");

        let verify_fail = runner
            .run_test(&base.clone().with_verify_cmd("mytool --version | grep -q 9.9.9"))
            .await
            .unwrap();
        assert_eq!(verify_fail.status, TestStatus::Failed);
        assert_eq!(verify_fail.error.as_ref().unwrap().category, "post_install");

        let verify_ok = runner
            .run_test(&base.with_verify_cmd("mytool --version | grep -q 1.2.3"))
            .await
            .unwrap();
        assert_eq!(verify_ok.status, TestStatus::Passed);
    }

    #[tokio::test]
    async fn test_local_capture_is_bounded() {
        let config = RunnerConfig {
            backend: ExecutionBackend::Local,
            max_capture_bytes: 64 * 1024,
            ..Default::default()
        };
        let runner = InstallerTestRunner::new(config);
        let dir = tempfile::tempdir().unwrap();
        let (url, sha) = file_fixture(
            dir.path(),
            "chatty",
            "#!/bin/bash\nhead -c 3000000 /dev/zero | tr '\\0' 'x'\necho\necho LAST_LINE_MARKER\nexit 0\n",
        );
        let test = InstallerTest::new("chatty", url)
            .with_sha256(sha)
            .with_timeout(Duration::from_secs(60));
        let result = runner.run_test(&test).await.unwrap();
        assert_eq!(result.status, TestStatus::Passed, "{}", result.stderr);
        assert!(
            result.stdout.len() < 64 * 1024 + 200,
            "stdout must be capped: {}",
            result.stdout.len()
        );
        assert!(result.stdout.contains("[afsc: truncated"), "marker present");
        assert!(result.stdout.trim_end().ends_with("LAST_LINE_MARKER"), "tail preserved");
    }

    #[tokio::test]
    async fn test_cancelled_token_short_circuits_before_start() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let config =
            RunnerConfig { backend: ExecutionBackend::Local, cancel, ..Default::default() };
        let runner = InstallerTestRunner::new(config);
        let test = InstallerTest::new("x", "https://127.0.0.1:9/x.sh").with_retry_count(3);
        let result = runner.run_test_with_retry(&test).await.unwrap();
        assert_eq!(result.status, TestStatus::Cancelled);
        assert_eq!(result.error.as_ref().unwrap().category, "cancelled");
        assert_eq!(result.attempts.len(), 1, "no retries after cancellation");
    }

    #[tokio::test]
    async fn test_local_installer_is_cancelled_mid_run() {
        let cancel = CancellationToken::new();
        let config = RunnerConfig {
            backend: ExecutionBackend::Local,
            cancel: cancel.clone(),
            ..Default::default()
        };
        let runner = InstallerTestRunner::new(config);
        let dir = tempfile::tempdir().unwrap();
        let (url, sha) = file_fixture(dir.path(), "sleep", "#!/bin/bash\nsleep 30\nexit 0\n");
        let test = InstallerTest::new("sleeper", url)
            .with_sha256(sha)
            .with_timeout(Duration::from_secs(60));

        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            canceller.cancel();
        });
        let start = Instant::now();
        let result = runner.run_test_with_retry(&test).await.unwrap();
        assert_eq!(result.status, TestStatus::Cancelled);
        // The script sleeps 30 s and the timeout is 60 s; anything well under that is prompt,
        // and the bound leaves room for a loaded test host.
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "cancellation must be prompt: {:?}",
            start.elapsed()
        );
        assert_eq!(result.error.as_ref().unwrap().category, "cancelled");
    }
}
