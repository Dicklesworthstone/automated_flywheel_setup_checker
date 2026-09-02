//! Installer test runner module

mod container;
mod executor;
mod installer;
mod parallel;
mod retry;

pub use container::{ContainerConfig, ContainerGuard, ContainerManager, PullPolicy};
pub use executor::{
    classify_result, finalize_failure, ExecutionBackend, InstallerTestRunner, RunnerConfig,
};
pub use installer::{
    tail, AttemptRecord, ChecksumResult, InstallerTest, RetryInfo, TestResult, TestStatus,
};
pub use parallel::ParallelRunner;
pub use retry::{RetryConfig, RetryStrategy};
