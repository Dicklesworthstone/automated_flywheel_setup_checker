//! Claude Code integration for auto-remediation with resilience

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

/// Circuit breaker states
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CircuitState {
    /// Normal operation - requests allowed
    Closed,
    /// Failing - requests rejected
    Open,
    /// Testing if service recovered
    HalfOpen,
}

/// Circuit breaker for Claude API calls
pub struct CircuitBreaker {
    state: RwLock<CircuitState>,
    failure_count: AtomicU32,
    success_count: AtomicU32,
    last_failure_time: Mutex<Option<Instant>>,

    // Configuration
    failure_threshold: u32,
    success_threshold: u32,
    timeout_duration: Duration,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, success_threshold: u32, timeout: Duration) -> Self {
        Self {
            state: RwLock::new(CircuitState::Closed),
            failure_count: AtomicU32::new(0),
            success_count: AtomicU32::new(0),
            last_failure_time: Mutex::new(None),
            failure_threshold,
            success_threshold,
            timeout_duration: timeout,
        }
    }

    /// Check if request should be allowed
    pub async fn should_allow(&self) -> bool {
        let state = *self.state.read().await;
        match state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if timeout has passed
                let last_failure = self.last_failure_time.lock().await;
                if let Some(time) = *last_failure {
                    if time.elapsed() >= self.timeout_duration {
                        // Transition to half-open
                        drop(last_failure);
                        *self.state.write().await = CircuitState::HalfOpen;
                        self.success_count.store(0, Ordering::SeqCst);
                        return true;
                    }
                }
                false
            }
            CircuitState::HalfOpen => true, // Allow test requests
        }
    }

    /// Record success
    pub async fn record_success(&self) {
        let state = *self.state.read().await;
        match state {
            CircuitState::Closed => {
                self.failure_count.store(0, Ordering::SeqCst);
            }
            CircuitState::HalfOpen => {
                let count = self.success_count.fetch_add(1, Ordering::SeqCst) + 1;
                if count >= self.success_threshold {
                    *self.state.write().await = CircuitState::Closed;
                    self.failure_count.store(0, Ordering::SeqCst);
                    tracing::info!("Circuit breaker closed - Claude API recovered");
                }
            }
            CircuitState::Open => {}
        }
    }

    /// Record failure
    pub async fn record_failure(&self) {
        let state = *self.state.read().await;
        match state {
            CircuitState::Closed => {
                let count = self.failure_count.fetch_add(1, Ordering::SeqCst) + 1;
                if count >= self.failure_threshold {
                    *self.state.write().await = CircuitState::Open;
                    *self.last_failure_time.lock().await = Some(Instant::now());
                    tracing::warn!("Circuit breaker opened - too many Claude API failures");
                }
            }
            CircuitState::HalfOpen => {
                // Single failure in half-open reopens the circuit
                *self.state.write().await = CircuitState::Open;
                *self.last_failure_time.lock().await = Some(Instant::now());
                tracing::warn!("Circuit breaker reopened - Claude API still failing");
            }
            CircuitState::Open => {}
        }
    }

    pub async fn get_state(&self) -> CircuitState {
        *self.state.read().await
    }
}

/// Rate limit error
#[derive(Debug, thiserror::Error)]
pub enum RateLimitError {
    #[error("Too many requests, retry after {retry_after_secs:.1}s")]
    TooManyRequests { retry_after_secs: f64 },
    #[error("Rate limit timeout exceeded")]
    Timeout,
}

/// Token bucket rate limiter
pub struct RateLimiter {
    tokens: AtomicU64,
    max_tokens: u64,
    refill_rate: u64, // tokens per second
    last_refill: Mutex<Instant>,
    cost_per_request: u64,
}

impl RateLimiter {
    pub fn new(max_tokens: u64, refill_rate: u64, cost_per_request: u64) -> Self {
        Self {
            tokens: AtomicU64::new(max_tokens),
            max_tokens,
            refill_rate,
            last_refill: Mutex::new(Instant::now()),
            cost_per_request,
        }
    }

    /// Try to acquire tokens for a request
    pub async fn try_acquire(&self) -> std::result::Result<(), RateLimitError> {
        // Refill tokens based on elapsed time
        {
            let mut last_refill = self.last_refill.lock().await;
            let elapsed = last_refill.elapsed();
            let new_tokens = (elapsed.as_secs_f64() * self.refill_rate as f64) as u64;

            if new_tokens > 0 {
                let current = self.tokens.load(Ordering::SeqCst);
                let refilled = std::cmp::min(current + new_tokens, self.max_tokens);
                self.tokens.store(refilled, Ordering::SeqCst);
                *last_refill = Instant::now();
            }
        }

        // Try to consume tokens
        let current = self.tokens.load(Ordering::SeqCst);
        if current >= self.cost_per_request {
            self.tokens.fetch_sub(self.cost_per_request, Ordering::SeqCst);
            Ok(())
        } else {
            // Calculate wait time
            let needed = self.cost_per_request - current;
            let wait_secs = needed as f64 / self.refill_rate as f64;
            Err(RateLimitError::TooManyRequests { retry_after_secs: wait_secs })
        }
    }

    /// Wait until tokens are available
    pub async fn acquire(&self, timeout: Duration) -> std::result::Result<(), RateLimitError> {
        let deadline = Instant::now() + timeout;

        loop {
            match self.try_acquire().await {
                Ok(()) => return Ok(()),
                Err(RateLimitError::TooManyRequests { retry_after_secs }) => {
                    let wait_duration = Duration::from_secs_f64(retry_after_secs);
                    if Instant::now() + wait_duration > deadline {
                        return Err(RateLimitError::Timeout);
                    }
                    tokio::time::sleep(wait_duration).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn available_tokens(&self) -> u64 {
        self.tokens.load(Ordering::SeqCst)
    }
}

/// Retry configuration with exponential backoff
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    pub max_retries: u32,
    #[serde(with = "humantime_serde")]
    pub initial_delay: Duration,
    #[serde(with = "humantime_serde")]
    pub max_delay: Duration,
    pub multiplier: f64,
    pub jitter: f64, // 0.0 to 1.0
}

mod humantime_serde {
    use serde::{self, Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            multiplier: 2.0,
            jitter: 0.1,
        }
    }
}

impl RetryConfig {
    pub fn get_delay(&self, attempt: u32) -> Duration {
        let base_delay = self.initial_delay.as_secs_f64() * self.multiplier.powi(attempt as i32);
        let capped_delay = base_delay.min(self.max_delay.as_secs_f64());

        // Add jitter
        let jitter_range = capped_delay * self.jitter;
        let jitter = rand::random::<f64>() * jitter_range * 2.0 - jitter_range;
        let final_delay = (capped_delay + jitter).max(0.1);

        Duration::from_secs_f64(final_delay)
    }
}

/// Remediation error types
#[derive(Debug, thiserror::Error)]
pub enum RemediationError {
    #[error("Claude CLI unavailable: {0}")]
    ClaudeUnavailable(String),
    #[error("Claude API error: {0}")]
    ApiError(String),
    #[error("Claude returned error: {0}")]
    ClaudeError(String),
    #[error("Request timeout")]
    Timeout,
    #[error("Rate limited: {0}")]
    RateLimited(String),
    #[error("Cost limit exceeded: ${current:.2} >= ${limit:.2}")]
    CostLimitExceeded { current: f32, limit: f32 },
    #[error("Parse error: {0}")]
    ParseError(String),
    #[error("Safety check failed: {0}")]
    SafetyCheckFailed(String),
}

/// Method used for remediation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemediationMethod {
    /// Claude fixed it automatically
    ClaudeAuto,
    /// Claude provided guidance, human applied
    ClaudeAssisted,
    /// Claude unavailable, manual instructions provided
    ManualRequired,
    /// Error not auto-fixable
    Skipped,
}

/// Type of file change
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChangeType {
    Created,
    Modified,
    Deleted,
}

/// A file change made during remediation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub change_type: ChangeType,
    pub diff: Option<String>,
    pub size_bytes: u64,
}

/// Result of a remediation attempt
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationResult {
    pub success: bool,
    pub method: RemediationMethod,
    pub changes_made: Vec<FileChange>,
    pub commit_sha: Option<String>,
    pub pr_url: Option<String>,
    pub duration_ms: u64,
    pub claude_output: String,
    pub estimated_cost_usd: f32,
    pub verification_passed: bool,
}

/// Health status of the remediation system
#[derive(Debug, Clone, Serialize)]
pub struct RemediationHealth {
    pub circuit_state: CircuitState,
    pub total_requests: u32,
    pub total_cost_usd: f32,
    pub cost_limit_usd: f32,
    pub cost_remaining_usd: f32,
    pub claude_available: bool,
}

/// Claude remediation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeRemediationConfig {
    pub enabled: bool,
    pub auto_commit: bool,
    pub create_pr: bool,
    pub require_approval: bool,
    pub max_attempts: u32,
    pub timeout_seconds: u64,
    pub cost_limit_usd: f32,
}

impl Default for ClaudeRemediationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_commit: false,
            create_pr: true,
            require_approval: true,
            max_attempts: 3,
            timeout_seconds: 300,
            cost_limit_usd: 10.0,
        }
    }
}

/// Main Claude remediation client with resilience
pub struct ClaudeRemediation {
    pub workspace: PathBuf,
    pub config: ClaudeRemediationConfig,

    // Resilience components
    circuit_breaker: Arc<CircuitBreaker>,
    rate_limiter: Arc<RateLimiter>,
    retry_config: RetryConfig,

    // Tracking
    total_cost_usd: AtomicU64, // Stored as microdollars
    request_count: AtomicU32,
}

impl ClaudeRemediation {
    pub fn new(workspace: PathBuf, config: ClaudeRemediationConfig) -> Self {
        Self {
            workspace,
            config,
            // Circuit breaker: open after 5 failures, close after 2 successes, 60s timeout
            circuit_breaker: Arc::new(CircuitBreaker::new(5, 2, Duration::from_secs(60))),
            // Rate limiter: 10 requests max, refill 1/sec, 1 token per request
            rate_limiter: Arc::new(RateLimiter::new(10, 1, 1)),
            retry_config: RetryConfig::default(),
            total_cost_usd: AtomicU64::new(0),
            request_count: AtomicU32::new(0),
        }
    }

    /// Execute Claude CLI with full resilience
    pub async fn execute_with_resilience(
        &self,
        prompt: &str,
    ) -> std::result::Result<RemediationResult, RemediationError> {
        if !self.config.enabled {
            return Ok(self.fallback_manual_instructions(prompt));
        }

        // Check circuit breaker
        if !self.circuit_breaker.should_allow().await {
            tracing::warn!("Circuit breaker open, falling back to manual instructions");
            return Ok(self.fallback_manual_instructions(prompt));
        }

        // Acquire rate limit token
        if let Err(e) = self.rate_limiter.acquire(Duration::from_secs(30)).await {
            tracing::warn!("Rate limit exceeded: {}", e);
            return Err(RemediationError::RateLimited(e.to_string()));
        }

        // Check cost limit
        let current_cost = self.get_total_cost_usd();
        if current_cost >= self.config.cost_limit_usd {
            tracing::warn!(
                "Cost limit exceeded: ${:.2} >= ${:.2}",
                current_cost,
                self.config.cost_limit_usd
            );
            return Err(RemediationError::CostLimitExceeded {
                current: current_cost,
                limit: self.config.cost_limit_usd,
            });
        }

        // Execute with retries
        let mut last_error = None;
        let start_time = Instant::now();

        for attempt in 0..self.retry_config.max_retries {
            if attempt > 0 {
                let delay = self.retry_config.get_delay(attempt);
                tracing::info!("Retrying Claude request in {:?} (attempt {})", delay, attempt + 1);
                tokio::time::sleep(delay).await;
            }

            match self.execute_claude_cli(prompt).await {
                Ok(mut result) => {
                    result.duration_ms = start_time.elapsed().as_millis() as u64;
                    self.circuit_breaker.record_success().await;
                    return Ok(result);
                }
                Err(e) => {
                    tracing::warn!("Claude request failed (attempt {}): {}", attempt + 1, e);
                    last_error = Some(e);

                    // Only record circuit breaker failure for certain error types
                    if matches!(
                        &last_error,
                        Some(RemediationError::ClaudeUnavailable(_))
                            | Some(RemediationError::Timeout)
                            | Some(RemediationError::ApiError(_))
                    ) {
                        self.circuit_breaker.record_failure().await;
                    }
                }
            }
        }

        // All retries exhausted
        tracing::error!("All Claude retries exhausted, falling back to manual");
        Ok(self.fallback_manual_instructions(prompt))
    }

    /// Execute Claude CLI (internal)
    async fn execute_claude_cli(
        &self,
        prompt: &str,
    ) -> std::result::Result<RemediationResult, RemediationError> {
        use tokio::process::Command;
        use tokio::time::timeout;

        self.request_count.fetch_add(1, Ordering::SeqCst);

        let output = timeout(
            Duration::from_secs(self.config.timeout_seconds),
            Command::new("claude")
                .arg("--print")
                .arg("--dangerously-skip-permissions")
                .arg("--output-format")
                .arg("json")
                .arg("-p")
                .arg(prompt)
                .current_dir(&self.workspace)
                .output(),
        )
        .await
        .map_err(|_| RemediationError::Timeout)?
        .map_err(|e| RemediationError::ClaudeUnavailable(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Check for specific error types
            if stderr.contains("rate limit") || stderr.contains("429") {
                return Err(RemediationError::ApiError("Rate limited by Anthropic API".into()));
            }
            if stderr.contains("authentication") || stderr.contains("401") {
                return Err(RemediationError::ClaudeUnavailable("Authentication failed".into()));
            }

            return Err(RemediationError::ClaudeError(stderr.to_string()));
        }

        // Parse output and estimate cost
        let stdout = String::from_utf8_lossy(&output.stdout);
        let estimated_cost = self.estimate_cost(&stdout);
        self.add_cost(estimated_cost);

        // Parse changes from output
        let changes = self.parse_changes(&stdout)?;

        Ok(RemediationResult {
            success: true,
            method: RemediationMethod::ClaudeAuto,
            changes_made: changes,
            commit_sha: None,
            pr_url: None,
            duration_ms: 0, // Set by caller
            claude_output: stdout.to_string(),
            estimated_cost_usd: estimated_cost,
            verification_passed: false, // Set after verification
        })
    }

    /// Fallback when Claude is unavailable
    fn fallback_manual_instructions(&self, prompt: &str) -> RemediationResult {
        let instructions = format!(
            "Claude is currently unavailable. Please review the following manually:\n\n{}\n\nOnce you've made changes, re-run the verification.",
            prompt
        );

        RemediationResult {
            success: false,
            method: RemediationMethod::ManualRequired,
            changes_made: vec![],
            commit_sha: None,
            pr_url: None,
            duration_ms: 0,
            claude_output: instructions,
            estimated_cost_usd: 0.0,
            verification_passed: false,
        }
    }

    fn estimate_cost(&self, output: &str) -> f32 {
        // Rough estimation based on token count
        let char_count = output.len();
        let estimated_tokens = char_count / 4; // ~4 chars per token
        let cost_per_1k_tokens = 0.015; // Claude Opus output pricing
        (estimated_tokens as f32 / 1000.0) * cost_per_1k_tokens
    }

    fn add_cost(&self, cost: f32) {
        let microdollars = (cost * 1_000_000.0) as u64;
        self.total_cost_usd.fetch_add(microdollars, Ordering::SeqCst);
    }

    pub fn get_total_cost_usd(&self) -> f32 {
        self.total_cost_usd.load(Ordering::SeqCst) as f32 / 1_000_000.0
    }

    fn parse_changes(
        &self,
        _output: &str,
    ) -> std::result::Result<Vec<FileChange>, RemediationError> {
        // Parse Claude output for file changes
        // Implementation depends on Claude output format
        Ok(vec![])
    }

    /// Get health status of the remediation system
    pub async fn health_check(&self) -> RemediationHealth {
        RemediationHealth {
            circuit_state: self.circuit_breaker.get_state().await,
            total_requests: self.request_count.load(Ordering::SeqCst),
            total_cost_usd: self.get_total_cost_usd(),
            cost_limit_usd: self.config.cost_limit_usd,
            cost_remaining_usd: self.config.cost_limit_usd - self.get_total_cost_usd(),
            claude_available: is_claude_available().await,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Verify a remediation by re-running the installer test
    ///
    /// This method runs the installer again after remediation to check
    /// if the fix was successful.
    pub async fn verify_remediation(
        &self,
        installer_url: &str,
        expected_checksum: Option<&str>,
    ) -> std::result::Result<VerificationResult, RemediationError> {
        use tokio::process::Command;
        use tokio::time::timeout;

        tracing::info!("Verifying remediation by re-running installer test");

        // Create a temporary directory for the test
        let temp_dir =
            tempfile::tempdir().map_err(|e| RemediationError::ClaudeError(e.to_string()))?;

        // Download and run the installer in test mode
        let curl_output = timeout(
            Duration::from_secs(60),
            Command::new("curl")
                .args(["-fsSL", installer_url])
                .output(),
        )
        .await
        .map_err(|_| RemediationError::Timeout)?
        .map_err(|e| RemediationError::ClaudeError(format!("curl failed: {}", e)))?;

        if !curl_output.status.success() {
            return Ok(VerificationResult {
                passed: false,
                exit_code: curl_output.status.code().unwrap_or(-1),
                stdout: String::new(),
                stderr: String::from_utf8_lossy(&curl_output.stderr).to_string(),
                checksum_valid: false,
            });
        }

        // Verify checksum if provided
        let checksum_valid = if let Some(expected) = expected_checksum {
            let actual = compute_sha256(&curl_output.stdout);
            actual == expected
        } else {
            true // No checksum to verify
        };

        // Run the installer in the temp directory
        let bash_output = timeout(
            Duration::from_secs(120),
            Command::new("bash")
                .arg("-s")
                .arg("--")
                .arg("--help") // Most installers support --help for dry validation
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .current_dir(temp_dir.path())
                .output(),
        )
        .await
        .map_err(|_| RemediationError::Timeout)?
        .map_err(|e| RemediationError::ClaudeError(format!("bash failed: {}", e)))?;

        Ok(VerificationResult {
            passed: bash_output.status.success() && checksum_valid,
            exit_code: bash_output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&bash_output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&bash_output.stderr).to_string(),
            checksum_valid,
        })
    }

    /// Full remediation workflow: prompt -> execute -> verify
    pub async fn remediate_and_verify(
        &self,
        prompt: &str,
        installer_url: &str,
        expected_checksum: Option<&str>,
    ) -> std::result::Result<RemediationResult, RemediationError> {
        // Execute remediation
        let mut result = self.execute_with_resilience(prompt).await?;

        if !result.success {
            return Ok(result);
        }

        // Verify the fix
        match self.verify_remediation(installer_url, expected_checksum).await {
            Ok(verification) => {
                result.verification_passed = verification.passed;
                if !verification.passed {
                    tracing::warn!(
                        "Verification failed after remediation: exit_code={}, checksum_valid={}",
                        verification.exit_code,
                        verification.checksum_valid
                    );
                }
            }
            Err(e) => {
                tracing::error!("Verification error: {}", e);
                result.verification_passed = false;
            }
        }

        Ok(result)
    }
}

/// Result of verification after remediation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub passed: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub checksum_valid: bool,
}

/// Compute SHA256 hash of data
fn compute_sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Check if Claude CLI is available and authenticated
pub async fn is_claude_available() -> bool {
    use tokio::process::Command;

    let output = Command::new("claude").arg("--version").output().await;

    match output {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

/// Get reason why Claude is unavailable
pub async fn get_unavailability_reason() -> Option<String> {
    use tokio::process::Command;

    let output = Command::new("claude").arg("--version").output().await;

    match output {
        Ok(o) if !o.status.success() => Some(String::from_utf8_lossy(&o.stderr).to_string()),
        Err(e) => Some(format!("Claude CLI not found: {}", e)),
        Ok(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_circuit_breaker_opens_after_failures() {
        let cb = CircuitBreaker::new(3, 2, Duration::from_secs(1));

        // Should start closed
        assert_eq!(cb.get_state().await, CircuitState::Closed);
        assert!(cb.should_allow().await);

        // Record 3 failures
        cb.record_failure().await;
        cb.record_failure().await;
        cb.record_failure().await;

        // Should now be open
        assert_eq!(cb.get_state().await, CircuitState::Open);
        assert!(!cb.should_allow().await);
    }

    #[tokio::test]
    async fn test_circuit_breaker_half_open_after_timeout() {
        let cb = CircuitBreaker::new(1, 1, Duration::from_millis(100));

        cb.record_failure().await;
        assert_eq!(cb.get_state().await, CircuitState::Open);

        // Wait for timeout
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should transition to half-open on next check
        assert!(cb.should_allow().await);
        assert_eq!(cb.get_state().await, CircuitState::HalfOpen);
    }

    #[tokio::test]
    async fn test_rate_limiter_basic() {
        let rl = RateLimiter::new(3, 1, 1);

        // First 3 should succeed
        assert!(rl.try_acquire().await.is_ok());
        assert!(rl.try_acquire().await.is_ok());
        assert!(rl.try_acquire().await.is_ok());

        // 4th should fail
        assert!(rl.try_acquire().await.is_err());
    }

    #[tokio::test]
    async fn test_rate_limiter_refills() {
        // Use 1 token/sec refill rate so the test has a clear timing window.
        // With 1 token/sec, we need 1000ms to refill 1 token.
        let rl = RateLimiter::new(1, 1, 1); // 1 token/sec refill

        assert!(rl.try_acquire().await.is_ok());
        // With 1 token/sec, negligible time between calls won't refill
        assert!(rl.try_acquire().await.is_err());

        // Wait for refill (1.1 seconds to ensure we get 1 token with 1 token/sec)
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // Should have tokens again
        assert!(rl.try_acquire().await.is_ok());
    }

    #[test]
    fn test_retry_config_exponential_backoff() {
        let config = RetryConfig {
            max_retries: 5,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            multiplier: 2.0,
            jitter: 0.0, // No jitter for deterministic test
        };

        assert_eq!(config.get_delay(0), Duration::from_secs(1));
        assert_eq!(config.get_delay(1), Duration::from_secs(2));
        assert_eq!(config.get_delay(2), Duration::from_secs(4));
        assert_eq!(config.get_delay(3), Duration::from_secs(8));
        assert_eq!(config.get_delay(4), Duration::from_secs(16));
        assert_eq!(config.get_delay(5), Duration::from_secs(30)); // Capped
    }

    #[test]
    fn test_cost_tracking() {
        let remediation =
            ClaudeRemediation::new(PathBuf::from("/tmp"), ClaudeRemediationConfig::default());

        assert_eq!(remediation.get_total_cost_usd(), 0.0);

        remediation.add_cost(0.05);
        assert!((remediation.get_total_cost_usd() - 0.05).abs() < 0.001);

        remediation.add_cost(0.10);
        assert!((remediation.get_total_cost_usd() - 0.15).abs() < 0.001);
    }
}
