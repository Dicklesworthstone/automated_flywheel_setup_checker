//! Parallel execution orchestrator
//!
//! Provides a worker pool that runs installer tests concurrently,
//! dispatching through the executor abstraction (Docker or local mode).

use super::executor::{InstallerTestRunner, RunnerConfig};
use super::installer::{InstallerTest, TestResult};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// gets its own container. Results are collected as they complete.
    pub async fn run_all(&self, tests: Vec<InstallerTest>) -> Result<Vec<TestResult>> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();

        for test in tests {
            let semaphore = self.semaphore.clone();
            let config = self.runner_config.clone();
            let cancelled = cancelled.clone();
            let fail_fast = self.fail_fast;

            let handle = tokio::spawn(async move {
                let _permit = semaphore.acquire().await.unwrap();

                // Check if we should skip due to fail-fast
                if cancelled.load(Ordering::Relaxed) {
                    return TestResult::new(&test.name).skipped("Skipped due to fail-fast");
                }

                let runner = InstallerTestRunner::new(config);
                let result = match runner.run_test_with_retry(&test).await {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(installer = %test.name, error = %e, "Test execution failed");
                        TestResult::new(&test.name)
                            .failed(-1, format!("Execution error: {}", e))
                    }
                };

                info!(
                    installer = %result.installer_name,
                    status = ?result.status,
                    duration_ms = result.duration_ms,
                    "Test completed"
                );

                // Signal cancellation on failure if fail-fast is enabled
                if fail_fast && !result.success {
                    cancelled.store(true, Ordering::Relaxed);
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

    #[tokio::test]
    async fn test_parallel_runner_creation() {
        let config = RunnerConfig {
            backend: ExecutionBackend::Local,
            ..Default::default()
        };
        let runner = ParallelRunner::new(4, config);
        assert_eq!(runner.max_parallel(), 4);
    }

    #[tokio::test]
    async fn test_parallel_runner_empty() {
        let config = RunnerConfig {
            backend: ExecutionBackend::Local,
            ..Default::default()
        };
        let runner = ParallelRunner::new(4, config);
        let results = runner.run_all(vec![]).await.unwrap();
        assert!(results.is_empty());
    }
}
