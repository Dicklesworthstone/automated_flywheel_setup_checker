//! Installer test runner module

pub mod acfs_profile;
mod container;
mod executor;
mod installer;
mod parallel;
mod retry;
pub mod spec;

pub use acfs_profile::{profile, Interpreter, Profile};
pub use container::{
    parse_memory_limit, ContainerConfig, ContainerGuard, ContainerManager, OrphanInfo, PullPolicy,
};
pub use spec::{resolve_spec, FieldSource, GlobalDefaults, InstallerSpec};
pub use executor::{
    classify_result, finalize_failure, ExecutionBackend, InstallerTestRunner, RunnerConfig,
};
pub use installer::{
    tail, AttemptRecord, ChecksumResult, ChecksumState, InstallerTest, RetryInfo, TestResult,
    TestStatus,
};
pub use parallel::ParallelRunner;
pub use retry::{RetryConfig, RetryStrategy};
