//! Retry logic with backoff
//!
//! Single source of truth for retry delays used by the executor. Delays grow exponentially
//! (or stay fixed), are capped, and can carry jitter so parallel workers do not retry in lockstep.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Retry strategy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RetryStrategy {
    /// Fixed delay between retries
    Fixed { delay: Duration },
    /// Exponential backoff
    Exponential { initial_delay: Duration, max_delay: Duration, multiplier: f64 },
}

impl Default for RetryStrategy {
    fn default() -> Self {
        RetryStrategy::Exponential {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            multiplier: 2.0,
        }
    }
}

/// Retry configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub strategy: RetryStrategy,
    pub retry_transient_only: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self { max_attempts: 3, strategy: RetryStrategy::default(), retry_transient_only: true }
    }
}

impl RetryConfig {
    /// The executor's default: 1s base, doubling, capped at 30s, four attempts total
    /// (README: `retry_transient = 3` retries after the first attempt).
    pub fn executor_default(retries: u32) -> Self {
        Self {
            max_attempts: retries.saturating_add(1).max(1),
            strategy: RetryStrategy::Exponential {
                initial_delay: Duration::from_secs(1),
                max_delay: Duration::from_secs(30),
                multiplier: 2.0,
            },
            retry_transient_only: true,
        }
    }

    /// Calculate delay for a given attempt number (attempt 0 = first retry wait when using
    /// the raw strategy; the executor passes the 1-based index of the attempt that just failed).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        match &self.strategy {
            RetryStrategy::Fixed { delay } => *delay,
            RetryStrategy::Exponential { initial_delay, max_delay, multiplier } => {
                let delay = initial_delay.as_secs_f64() * multiplier.powi(attempt.min(30) as i32);
                Duration::from_secs_f64(delay.min(max_delay.as_secs_f64()))
            }
        }
    }

    /// Delay with additive jitter in `[0, fraction * delay]`, still capped by the strategy's
    /// maximum delay.
    pub fn delay_with_jitter(&self, attempt: u32, fraction: f64) -> Duration {
        let base = self.delay_for_attempt(attempt);
        if fraction <= 0.0 {
            return base;
        }
        let jitter = rand::random::<f64>() * base.as_secs_f64() * fraction.clamp(0.0, 1.0);
        let total = Duration::from_secs_f64(base.as_secs_f64() + jitter);
        match &self.strategy {
            RetryStrategy::Exponential { max_delay, .. } => total.min(*max_delay),
            RetryStrategy::Fixed { .. } => total,
        }
    }

    /// Check if we should retry
    pub fn should_retry(&self, attempt: u32) -> bool {
        attempt < self.max_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixed_strategy() {
        let config = RetryConfig {
            max_attempts: 3,
            strategy: RetryStrategy::Fixed { delay: Duration::from_secs(5) },
            retry_transient_only: true,
        };

        assert_eq!(config.delay_for_attempt(0), Duration::from_secs(5));
        assert_eq!(config.delay_for_attempt(2), Duration::from_secs(5));
    }

    #[test]
    fn test_exponential_strategy() {
        let config = RetryConfig::default();

        // First retry: 1 * 2^0 = 1s
        assert_eq!(config.delay_for_attempt(0), Duration::from_secs(1));
        // Second retry: 1 * 2^1 = 2s
        assert_eq!(config.delay_for_attempt(1), Duration::from_secs(2));
        // Third retry: 1 * 2^2 = 4s
        assert_eq!(config.delay_for_attempt(2), Duration::from_secs(4));
    }

    #[test]
    fn test_should_retry() {
        let config = RetryConfig { max_attempts: 3, ..Default::default() };

        assert!(config.should_retry(0));
        assert!(config.should_retry(2));
        assert!(!config.should_retry(3));
    }

    #[test]
    fn test_executor_default_attempts_and_cap() {
        let config = RetryConfig::executor_default(3);
        assert_eq!(config.max_attempts, 4);
        assert_eq!(config.delay_for_attempt(1), Duration::from_secs(2));
        assert_eq!(config.delay_for_attempt(2), Duration::from_secs(4));
        assert_eq!(config.delay_for_attempt(10), Duration::from_secs(30));
        assert_eq!(RetryConfig::executor_default(0).max_attempts, 1);
    }

    #[test]
    fn test_jitter_bounds() {
        let config = RetryConfig::executor_default(3);
        for _ in 0..50 {
            let d = config.delay_with_jitter(1, 0.25);
            assert!(d >= Duration::from_secs(2), "{d:?}");
            assert!(d <= Duration::from_millis(2500), "{d:?}");
        }
        // Jitter never exceeds the cap.
        let d = config.delay_with_jitter(10, 0.25);
        assert_eq!(d, Duration::from_secs(30));
        // Zero fraction is deterministic.
        assert_eq!(config.delay_with_jitter(1, 0.0), Duration::from_secs(2));
    }
}
