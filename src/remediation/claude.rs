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

    /// Current breaker state (used by tests).
    #[cfg(test)]
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
    /// The CLI stopped on its own `--max-budget-usd` / `--max-turns` cap; retrying cannot help
    #[error("Claude stopped: {0}")]
    CapReached(String),
}

/// Method used for remediation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

/// Result of a remediation attempt
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationResult {
    pub success: bool,
    pub method: RemediationMethod,
    pub commit_sha: Option<String>,
    pub pr_url: Option<String>,
    pub duration_ms: u64,
    pub claude_output: String,
    pub estimated_cost_usd: f32,
    pub verification_passed: bool,
    /// Parsed `claude --print --output-format json` envelope (None for fallbacks)
    #[serde(default)]
    pub envelope: Option<ClaudeEnvelope>,
}

/// The `claude --print --output-format json` envelope (fields we rely on).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClaudeEnvelope {
    /// `null` on budget/turn-cap envelopes (Claude Code 2.1.x); read as empty
    #[serde(default, deserialize_with = "null_as_default")]
    pub result: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub total_cost_usd: f64,
    #[serde(default)]
    pub num_turns: u32,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub subtype: Option<String>,
    /// Error strings the CLI attaches to `is_error` envelopes (e.g. "Reached maximum budget ($0.05)")
    #[serde(default, deserialize_with = "null_as_default")]
    pub errors: Vec<String>,
}

/// Accept JSON `null` where the CLI leaves a field empty instead of omitting it.
fn null_as_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

impl ClaudeEnvelope {
    /// Human reason for an `is_error` envelope: the CLI's own error strings, else the result
    /// text, else the subtype.
    pub fn error_reason(&self) -> String {
        if !self.errors.is_empty() {
            return self.errors.join("; ");
        }
        if !self.result.is_empty() && self.result != "null" {
            return self.result.clone();
        }
        self.subtype.clone().unwrap_or_else(|| "claude reported is_error=true".into())
    }

    /// `error_max_budget_usd` / `error_max_turns`: the CLI hit a cap the operator set.
    pub fn hit_cap(&self) -> bool {
        self.subtype.as_deref().is_some_and(|s| s.starts_with("error_max_"))
    }
}

/// Interpret one CLI run. The envelope is on stdout even when the exit status is non-zero
/// (budget/turn caps exit 1 with an `is_error` envelope and nothing on stderr), so it is parsed
/// first and its cost is always accounted.
fn interpret_run(status_ok: bool, stdout: &str, stderr: &str) -> std::result::Result<ClaudeEnvelope, (RemediationError, f32)> {
    let envelope = ClaudeEnvelope::parse(stdout);
    let cost = envelope.total_cost_usd as f32;
    let looks_like_envelope = stdout.trim_start().starts_with('{');
    if envelope.is_error {
        let reason = format!("{} (after {} turn(s), ${:.4})", envelope.error_reason(), envelope.num_turns, envelope.total_cost_usd);
        let err = if envelope.hit_cap() { RemediationError::CapReached(reason) } else { RemediationError::ClaudeError(reason) };
        return Err((err, cost));
    }
    if !status_ok {
        let stderr = stderr.trim();
        if stderr.contains("rate limit") || stderr.contains("429") {
            return Err((RemediationError::ApiError("Rate limited by Anthropic API".into()), cost));
        }
        if stderr.contains("authentication") || stderr.contains("401") {
            return Err((RemediationError::ClaudeUnavailable("Authentication failed".into()), cost));
        }
        let detail = if !stderr.is_empty() {
            stderr.to_string()
        } else if looks_like_envelope {
            format!("exit status non-zero with a non-error envelope (subtype {:?})", envelope.subtype)
        } else {
            format!("exit status non-zero, no envelope; stdout: {}", stdout.trim().chars().take(300).collect::<String>())
        };
        return Err((RemediationError::ClaudeError(detail), cost));
    }
    Ok(envelope)
}

impl ClaudeEnvelope {
    /// Parse stdout; tolerates leading log lines and a plain-text result (treated as `result`).
    pub fn parse(stdout: &str) -> Self {
        let trimmed = stdout.trim();
        if let Ok(env) = serde_json::from_str::<ClaudeEnvelope>(trimmed) {
            return env;
        }
        // Stream-json or noisy output: take the last line that parses as an envelope.
        for line in trimmed.lines().rev() {
            if let Ok(env) = serde_json::from_str::<ClaudeEnvelope>(line.trim()) {
                if !env.result.is_empty() || env.is_error {
                    return env;
                }
            }
        }
        ClaudeEnvelope { result: trimmed.to_string(), ..Default::default() }
    }
}

/// Arguments for a read-only advisory run. Read-only is enforced by the tool list (no Edit,
/// Write or Bash); the permission mode stays `default` because `plan` makes the CLI spend turns
/// trying to write a plan file and call ExitPlanMode, which are not available here (observed
/// with Claude Code 2.1.259). Never includes `--dangerously-skip-permissions`.
pub fn advisory_args(max_turns: u32, max_budget_usd: f32, prompt: &str) -> Vec<String> {
    vec![
        "--print".into(),
        "--output-format".into(),
        "json".into(),
        "--permission-mode".into(),
        "default".into(),
        "--tools".into(),
        "Read,Grep,Glob".into(),
        "--max-turns".into(),
        max_turns.max(1).to_string(),
        "--max-budget-usd".into(),
        format!("{max_budget_usd:.2}"),
        "-p".into(),
        prompt.to_string(),
    ]
}


/// Arguments for a propose/apply edit session: edits are accepted without prompts but only
/// inside `worktree` (`--add-dir`), Bash only when the operator opted in. Never includes
/// `--dangerously-skip-permissions`.
pub fn edit_args(max_turns: u32, max_budget_usd: f32, worktree: &std::path::Path, allow_bash: bool, prompt: &str) -> Vec<String> {
    let tools = if allow_bash { "Read,Grep,Glob,Edit,Write,Bash" } else { "Read,Grep,Glob,Edit,Write" };
    vec![
        "--print".into(),
        "--output-format".into(),
        "json".into(),
        "--permission-mode".into(),
        "acceptEdits".into(),
        "--tools".into(),
        tools.into(),
        "--add-dir".into(),
        worktree.to_string_lossy().to_string(),
        "--max-turns".into(),
        max_turns.max(1).to_string(),
        "--max-budget-usd".into(),
        format!("{max_budget_usd:.2}"),
        "-p".into(),
        prompt.to_string(),
    ]
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
    /// Agent turns per invocation
    pub max_turns: u32,
    /// Binary to invoke (None = `claude` on PATH); tests point this at a fake
    pub claude_bin: Option<String>,
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
            max_turns: 8,
            claude_bin: None,
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

                    // A budget or turn cap is the operator's setting, not a transient fault:
                    // retrying would only spend the same money again.
                    if matches!(&e, RemediationError::CapReached(_) | RemediationError::CostLimitExceeded { .. }) {
                        return Err(e);
                    }
                    // Only record circuit breaker failure for certain error types
                    if matches!(
                        &e,
                        RemediationError::ClaudeUnavailable(_)
                            | RemediationError::Timeout
                            | RemediationError::ApiError(_)
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

    /// Execute Claude CLI (internal): read-only advisory invocation. Never grants edit
    /// permissions; the envelope is the source of truth for cost and errors.
    async fn execute_claude_cli(
        &self,
        prompt: &str,
    ) -> std::result::Result<RemediationResult, RemediationError> {
        use tokio::process::Command;
        use tokio::time::timeout;

        self.request_count.fetch_add(1, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let bin = self.config.claude_bin.clone().unwrap_or_else(|| "claude".to_string());
        let remaining_budget = (self.config.cost_limit_usd - self.get_total_cost_usd()).max(0.01);

        let output = timeout(
            Duration::from_secs(self.config.timeout_seconds),
            Command::new(&bin)
                .args(advisory_args(self.config.max_turns, remaining_budget, prompt))
                .current_dir(&self.workspace)
                .output(),
        )
        .await
        .map_err(|_| RemediationError::Timeout)?
        .map_err(|e| RemediationError::ClaudeUnavailable(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let envelope = match interpret_run(output.status.success(), &stdout, &stderr) {
            Ok(env) => env,
            Err((e, cost)) => {
                self.add_cost(cost);
                return Err(e);
            }
        };
        self.add_cost(envelope.total_cost_usd as f32);

        Ok(RemediationResult {
            success: true,
            method: RemediationMethod::ClaudeAuto,
            commit_sha: None,
            pr_url: None,
            duration_ms: started.elapsed().as_millis() as u64,
            claude_output: envelope.result.clone(),
            estimated_cost_usd: envelope.total_cost_usd as f32,
            verification_passed: false,
            envelope: Some(envelope),
        })
    }

    /// One propose/apply edit session with cwd = `worktree`. No automatic retries: an edit
    /// session is not idempotent, the caller bounds attempts with `max_attempts` and decides
    /// what to do with the worktree. Rate limit, circuit breaker and cost cap still apply.
    pub async fn execute_edit_session(
        &self,
        prompt: &str,
        worktree: &std::path::Path,
        allow_bash: bool,
    ) -> std::result::Result<RemediationResult, RemediationError> {
        use tokio::process::Command;
        use tokio::time::timeout;

        if !self.config.enabled {
            return Err(RemediationError::ClaudeUnavailable("remediation disabled".into()));
        }
        if !self.circuit_breaker.should_allow().await {
            return Err(RemediationError::ClaudeUnavailable("circuit breaker open".into()));
        }
        if let Err(e) = self.rate_limiter.acquire(Duration::from_secs(30)).await {
            return Err(RemediationError::RateLimited(e.to_string()));
        }
        let current_cost = self.get_total_cost_usd();
        if current_cost >= self.config.cost_limit_usd {
            return Err(RemediationError::CostLimitExceeded { current: current_cost, limit: self.config.cost_limit_usd });
        }
        self.request_count.fetch_add(1, Ordering::SeqCst);
        let started = Instant::now();
        let bin = self.config.claude_bin.clone().unwrap_or_else(|| "claude".to_string());
        let remaining_budget = (self.config.cost_limit_usd - current_cost).max(0.01);
        let output = timeout(
            Duration::from_secs(self.config.timeout_seconds),
            Command::new(&bin)
                .args(edit_args(self.config.max_turns, remaining_budget, worktree, allow_bash, prompt))
                .current_dir(worktree)
                .output(),
        )
        .await
        .map_err(|_| RemediationError::Timeout)?
        .map_err(|e| RemediationError::ClaudeUnavailable(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let envelope = match interpret_run(output.status.success(), &stdout, &stderr) {
            Ok(env) => env,
            Err((e, cost)) => {
                self.add_cost(cost);
                if matches!(&e, RemediationError::ClaudeUnavailable(_) | RemediationError::ApiError(_)) {
                    self.circuit_breaker.record_failure().await;
                }
                return Err(e);
            }
        };
        self.add_cost(envelope.total_cost_usd as f32);
        self.circuit_breaker.record_success().await;
        Ok(RemediationResult {
            success: true,
            method: RemediationMethod::ClaudeAuto,
            commit_sha: None,
            pr_url: None,
            duration_ms: started.elapsed().as_millis() as u64,
            claude_output: envelope.result.clone(),
            estimated_cost_usd: envelope.total_cost_usd as f32,
            verification_passed: false,
            envelope: Some(envelope),
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
            commit_sha: None,
            pr_url: None,
            duration_ms: 0,
            claude_output: instructions,
            estimated_cost_usd: 0.0,
            verification_passed: false,
            envelope: None,
        }
    }

    fn add_cost(&self, cost: f32) {
        let microdollars = (cost * 1_000_000.0) as u64;
        self.total_cost_usd.fetch_add(microdollars, Ordering::SeqCst);
    }

    /// Whether Claude invocations are enabled at all.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn get_total_cost_usd(&self) -> f32 {
        self.total_cost_usd.load(Ordering::SeqCst) as f32 / 1_000_000.0
    }

    /// Same total, exact to the microdollar (for outcomes and metrics; no f32 noise).
    pub fn total_cost_usd_exact(&self) -> f64 {
        self.total_cost_usd.load(Ordering::SeqCst) as f64 / 1_000_000.0
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
    fn budget_cap_envelopes_are_errors_with_the_real_reason_and_cost() {
        // Pinned from `claude --print --output-format json --max-budget-usd 0.05 …` (2.1.259):
        // exit 1, nothing on stderr, and this on stdout.
        let stdout = r#"{"type":"result","subtype":"error_max_budget_usd","is_error":true,"duration_ms":1500,"num_turns":1,"result":null,"session_id":"57e7","total_cost_usd":0.128202,"errors":["Reached maximum budget ($0.05)"]}"#;
        match interpret_run(false, stdout, "") {
            Err((RemediationError::CapReached(reason), cost)) => {
                assert!(reason.contains("Reached maximum budget ($0.05)"), "{reason}");
                assert!(reason.contains("$0.1282"), "{reason}");
                assert!((cost - 0.128202).abs() < 1e-5);
            }
            other => panic!("expected a cap error, got {other:?}"),
        }
        // Other is_error envelopes stay ordinary (retryable) errors; a clean run passes through.
        let plain = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":2,"result":"boom","total_cost_usd":0.01}"#;
        assert!(matches!(interpret_run(true, plain, ""), Err((RemediationError::ClaudeError(r), _)) if r.starts_with("boom")));
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"fine","total_cost_usd":0.2}"#;
        assert_eq!(interpret_run(true, ok, "").unwrap().result, "fine");
        // Non-zero exit without an envelope keeps stderr, or says so when both are empty.
        assert!(matches!(interpret_run(false, "", "boom on stderr"), Err((RemediationError::ClaudeError(r), _)) if r == "boom on stderr"));
        assert!(matches!(interpret_run(false, "", ""), Err((RemediationError::ClaudeError(r), _)) if r.contains("no envelope")));
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
