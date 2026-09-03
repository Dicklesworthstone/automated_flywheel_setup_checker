//! Parallel execution orchestrator
//!
//! Provides a worker pool that runs installer tests concurrently,
//! dispatching through the executor abstraction (Docker or local mode).
//!
//! Cancellation: the pool owns a child of the run's [`CancellationToken`]. Fail-fast cancels it
//! on the first failure (in-flight installers are stopped and reported `Cancelled`, queued ones
//! `Skipped`); a signal cancels the parent and produces the same shape with `Cancelled`.

use super::executor::{finalize_failure, InstallerTestRunner, RunnerConfig};
use super::installer::{InstallerTest, TestResult, TestStatus};
use crate::parser::CANCELLED_MARKER;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{info, warn};

/// Orchestrates parallel installer test execution
pub struct ParallelRunner {
    max_parallel: usize,
    semaphore: Arc<Semaphore>,
    runner_config: RunnerConfig,
    fail_fast: bool,
}

impl ParallelRunner {
    pub fn new(max_parallel: usize, runner_config: RunnerConfig) -> Self {
        let max_parallel = max_parallel.max(1);
        Self {
            max_parallel,
            semaphore: Arc::new(Semaphore::new(max_parallel)),
            runner_config,
            fail_fast: false,
        }
    }

    /// Enable fail-fast mode (stop after first failure)
    pub fn with_fail_fast(mut self, fail_fast: bool) -> Self {
        self.fail_fast = fail_fast;
        self
    }

    /// Run multiple installer tests in parallel
    ///
    /// Each worker gets its own executor instance. In Docker mode, each test
    /// gets its own container. Results are collected in submission order.
    pub async fn run_all(&self, tests: Vec<InstallerTest>) -> Result<Vec<TestResult>> {
        // The pool token is a child of the run token: a signal cancels both; fail-fast cancels
        // only the pool.
        let run_token = self.runner_config.cancel.clone();
        let pool_token = run_token.child_token();
        let mut handles = Vec::new();

        for test in tests {
            let semaphore = self.semaphore.clone();
            let mut config = self.runner_config.clone();
            config.cancel = pool_token.clone();
            let pool_token = pool_token.clone();
            let run_token = run_token.clone();
            let fail_fast = self.fail_fast;

            let handle = tokio::spawn(async move {
                let _permit = semaphore.acquire().await.unwrap();

                // Queued work after cancellation: a signal yields Cancelled, fail-fast yields Skipped.
                if run_token.is_cancelled() {
                    let mut r = TestResult::new(&test.name)
                        .cancelled(format!("{CANCELLED_MARKER} before start"));
                    finalize_failure(&mut r, None);
                    return r;
                }
                if pool_token.is_cancelled() {
                    return TestResult::new(&test.name).skipped("Skipped due to fail-fast");
                }

                let runner = InstallerTestRunner::new(config);
                let result = match runner.run_test_with_retry(&test).await {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(installer = %test.name, error = %e, "Test execution failed");
                        let mut r = TestResult::new(&test.name)
                            .failed(-1, format!("Execution error: {}", e));
                        finalize_failure(&mut r, None);
                        r
                    }
                };

                info!(
                    installer = %result.installer_name,
                    status = ?result.status,
                    duration_ms = result.duration_ms,
                    "Test completed"
                );

                // Signal cancellation on failure if fail-fast is enabled (not for cancellations
                // caused by the run token itself).
                if fail_fast && !result.success && result.status != TestStatus::Cancelled {
                    pool_token.cancel();
                }

                result
            });
            handles.push(handle);
        }

        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => {
                    warn!(error = %e, "Worker task panicked");
                }
            }
        }

        Ok(results)
    }

    pub fn max_parallel(&self) -> usize {
        self.max_parallel
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::executor::ExecutionBackend;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn test_parallel_runner_creation() {
        let config = RunnerConfig { backend: ExecutionBackend::Local, ..Default::default() };
        let runner = ParallelRunner::new(4, config);
        assert_eq!(runner.max_parallel(), 4);
    }

    #[tokio::test]
    async fn test_parallel_runner_empty() {
        let config = RunnerConfig { backend: ExecutionBackend::Local, ..Default::default() };
        let runner = ParallelRunner::new(4, config);
        let results = runner.run_all(vec![]).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_parallel_runner_zero_is_clamped() {
        let config = RunnerConfig { backend: ExecutionBackend::Local, ..Default::default() };
        let runner = ParallelRunner::new(0, config);
        assert_eq!(runner.max_parallel(), 1);
    }

    #[tokio::test]
    async fn test_cancelled_run_token_marks_queued_tests_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let config =
            RunnerConfig { backend: ExecutionBackend::Local, cancel, ..Default::default() };
        let runner = ParallelRunner::new(2, config);
        let tests = vec![
            InstallerTest::new("a", "https://127.0.0.1:9/a.sh"),
            InstallerTest::new("b", "https://127.0.0.1:9/b.sh"),
        ];
        let results = runner.run_all(tests).await.unwrap();
        assert_eq!(results.len(), 2);
        for r in &results {
            assert_eq!(r.status, TestStatus::Cancelled);
            assert_eq!(r.error.as_ref().unwrap().category, "cancelled");
        }
    }
}
