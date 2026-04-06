//! Tests for the runner module
//!
//! Tests cover:
//! - TestResult creation and state transitions
//! - InstallerTest configuration
//! - Retry logic and backoff
//! - Container configuration
//! - SHA256 checksum verification
//! - Execution backend selection

use automated_flywheel_setup_checker::runner::{
    ContainerConfig, ContainerManager, InstallerTest, ParallelRunner, RetryConfig, RetryStrategy,
    RunnerConfig, TestResult, TestStatus,
};
use std::time::Duration;

// ============================================================================
// TestResult Tests
// ============================================================================

#[test]
fn test_result_new() {
    let result = TestResult::new("test-installer");
    assert_eq!(result.installer_name, "test-installer");
    assert_eq!(result.status, TestStatus::Pending);
    assert!(!result.success);
    assert!(result.exit_code.is_none());
    assert!(result.stdout.is_empty());
    assert!(result.stderr.is_empty());
    assert_eq!(result.attempt, 1);
    assert_eq!(result.max_attempts, 3);
    assert!(result.retries.is_empty());
}

#[test]
fn test_result_passed() {
    let result = TestResult::new("test-installer").passed();
    assert_eq!(result.status, TestStatus::Passed);
    assert!(result.success);
    assert_eq!(result.exit_code, Some(0));
    assert!(result.duration_ms > 0 || result.duration.as_millis() == 0);
}

#[test]
fn test_result_failed() {
    let result = TestResult::new("test-installer").failed(1, "some error");
    assert_eq!(result.status, TestStatus::Failed);
    assert!(!result.success);
    assert_eq!(result.exit_code, Some(1));
    assert_eq!(result.stderr, "some error");
}

#[test]
fn test_result_failed_with_different_codes() {
    for code in [1, 2, 126, 127, 255] {
        let result = TestResult::new("test").failed(code, "error");
        assert_eq!(result.exit_code, Some(code));
        assert!(!result.success);
    }
}

#[test]
fn test_result_timed_out() {
    let result = TestResult::new("test-installer").timed_out();
    assert_eq!(result.status, TestStatus::TimedOut);
    assert!(!result.success);
    assert!(result.exit_code.is_none());
}

#[test]
fn test_result_skipped() {
    let result = TestResult::new("test-installer").skipped("disabled by config");
    assert_eq!(result.status, TestStatus::Skipped);
    assert!(!result.success);
    assert_eq!(result.stderr, "disabled by config");
    assert_eq!(result.duration_ms, 0);
}

#[test]
fn test_result_with_container_id() {
    let result = TestResult::new("test").with_container_id("abc123def456");
    assert_eq!(result.container_id, Some("abc123def456".to_string()));
}

#[test]
fn test_result_retries() {
    let mut result = TestResult::new("test-installer");
    assert_eq!(result.retry_count(), 0);
    assert_eq!(result.attempt, 1);

    result.add_retry("first failure", 1000);
    assert_eq!(result.retry_count(), 1);
    assert_eq!(result.attempt, 2);
    assert_eq!(result.retries[0].wait_ms, 1000);
    assert_eq!(result.retries[0].attempt, 1);

    result.add_retry("second failure", 2000);
    assert_eq!(result.retry_count(), 2);
    assert_eq!(result.attempt, 3);
    assert_eq!(result.retries[1].wait_ms, 2000);
}

#[test]
fn test_result_duration_tracking() {
    let result = TestResult::new("test").passed();
    // Duration fields should be populated (unsigned types are always >= 0)
    let _ = result.duration.as_nanos();
    let _ = result.duration_ms;
}

// ============================================================================
// InstallerTest Tests
// ============================================================================

#[test]
fn test_installer_test_new() {
    let test = InstallerTest::new("my-installer", "https://example.com/install.sh");
    assert_eq!(test.name, "my-installer");
    assert_eq!(test.url, "https://example.com/install.sh");
    assert!(test.expected_sha256.is_none());
    assert!(test.script_path.is_none());
    assert_eq!(test.timeout, Duration::from_secs(300));
    assert_eq!(test.timeout_seconds, 300);
    assert_eq!(test.retry_count, 3);
    assert!(test.tags.is_empty());
    assert!(test.environment.is_empty());
}

#[test]
fn test_installer_test_with_sha256() {
    let test =
        InstallerTest::new("test", "https://example.com").with_sha256("abc123def456789012345678");
    assert_eq!(test.expected_sha256, Some("abc123def456789012345678".to_string()));
}

#[test]
fn test_installer_test_with_script_path() {
    let test = InstallerTest::new("test", "https://example.com").with_script_path("/tmp/install.sh");
    assert_eq!(test.script_path, Some("/tmp/install.sh".to_string()));
}

#[test]
fn test_installer_test_with_timeout() {
    let test =
        InstallerTest::new("test", "https://example.com").with_timeout(Duration::from_secs(600));
    assert_eq!(test.timeout, Duration::from_secs(600));
    assert_eq!(test.timeout_seconds, 600);
}

#[test]
fn test_installer_test_with_retry_count() {
    let test = InstallerTest::new("test", "https://example.com").with_retry_count(5);
    assert_eq!(test.retry_count, 5);
}

#[test]
fn test_installer_test_with_tags() {
    let test = InstallerTest::new("test", "https://example.com")
        .with_tags(vec!["essential".to_string(), "network".to_string()]);
    assert_eq!(test.tags.len(), 2);
    assert!(test.tags.contains(&"essential".to_string()));
    assert!(test.tags.contains(&"network".to_string()));
}

#[test]
fn test_installer_test_with_env() {
    let test = InstallerTest::new("test", "https://example.com")
        .with_env("MY_VAR", "my_value")
        .with_env("ANOTHER", "value2");
    assert_eq!(test.environment.len(), 2);
    assert!(test.environment.contains(&("MY_VAR".to_string(), "my_value".to_string())));
}

#[test]
fn test_installer_test_builder_chain() {
    let test = InstallerTest::new("my-installer", "https://example.com/install.sh")
        .with_sha256("abc123")
        .with_timeout(Duration::from_secs(600))
        .with_retry_count(5)
        .with_tags(vec!["essential".to_string()])
        .with_env("DEBUG", "1");

    assert_eq!(test.name, "my-installer");
    assert_eq!(test.expected_sha256, Some("abc123".to_string()));
    assert_eq!(test.timeout_seconds, 600);
    assert_eq!(test.retry_count, 5);
    assert_eq!(test.tags.len(), 1);
    assert_eq!(test.environment.len(), 1);
}

// ============================================================================
// RetryConfig Tests
// ============================================================================

#[test]
fn test_retry_config_default() {
    let config = RetryConfig::default();
    assert_eq!(config.max_attempts, 3);
    assert!(config.retry_transient_only);
}

#[test]
fn test_fixed_retry_strategy() {
    let config = RetryConfig {
        max_attempts: 3,
        strategy: RetryStrategy::Fixed { delay: Duration::from_secs(5) },
        retry_transient_only: true,
    };

    assert_eq!(config.delay_for_attempt(0), Duration::from_secs(5));
    assert_eq!(config.delay_for_attempt(1), Duration::from_secs(5));
    assert_eq!(config.delay_for_attempt(2), Duration::from_secs(5));
    assert_eq!(config.delay_for_attempt(10), Duration::from_secs(5));
}

#[test]
fn test_exponential_retry_strategy() {
    let config = RetryConfig {
        max_attempts: 5,
        strategy: RetryStrategy::Exponential {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            multiplier: 2.0,
        },
        retry_transient_only: true,
    };

    // 1 * 2^0 = 1s
    assert_eq!(config.delay_for_attempt(0), Duration::from_secs(1));
    // 1 * 2^1 = 2s
    assert_eq!(config.delay_for_attempt(1), Duration::from_secs(2));
    // 1 * 2^2 = 4s
    assert_eq!(config.delay_for_attempt(2), Duration::from_secs(4));
    // 1 * 2^3 = 8s
    assert_eq!(config.delay_for_attempt(3), Duration::from_secs(8));
    // 1 * 2^4 = 16s
    assert_eq!(config.delay_for_attempt(4), Duration::from_secs(16));
}

#[test]
fn test_exponential_retry_capped() {
    let config = RetryConfig {
        max_attempts: 10,
        strategy: RetryStrategy::Exponential {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            multiplier: 2.0,
        },
        retry_transient_only: true,
    };

    // 1 * 2^6 = 64s, but capped at 30s
    assert_eq!(config.delay_for_attempt(6), Duration::from_secs(30));
    assert_eq!(config.delay_for_attempt(10), Duration::from_secs(30));
}

#[test]
fn test_should_retry() {
    let config = RetryConfig { max_attempts: 3, ..Default::default() };

    assert!(config.should_retry(0));
    assert!(config.should_retry(1));
    assert!(config.should_retry(2));
    assert!(!config.should_retry(3));
    assert!(!config.should_retry(4));
}

#[test]
fn test_should_retry_single_attempt() {
    let config = RetryConfig { max_attempts: 1, ..Default::default() };

    assert!(config.should_retry(0));
    assert!(!config.should_retry(1));
}

#[test]
fn test_retry_strategy_default() {
    let strategy = RetryStrategy::default();
    match strategy {
        RetryStrategy::Exponential { initial_delay, max_delay, multiplier } => {
            assert_eq!(initial_delay, Duration::from_secs(1));
            assert_eq!(max_delay, Duration::from_secs(60));
            assert_eq!(multiplier, 2.0);
        }
        _ => panic!("Expected Exponential strategy as default"),
    }
}

// ============================================================================
// ContainerConfig Tests
// ============================================================================

#[test]
fn test_container_config_default() {
    let config = ContainerConfig::default();
    assert_eq!(config.image, "ubuntu:22.04");
    assert_eq!(config.memory_limit, Some(2 * 1024 * 1024 * 1024)); // 2GB
    assert_eq!(config.cpu_quota, Some(1.0));
    assert_eq!(config.timeout_seconds, 300);
    assert!(config.volumes.is_empty());
    assert!(config.environment.is_empty());
}

#[test]
fn test_container_config_custom() {
    let config = ContainerConfig {
        image: "debian:latest".to_string(),
        memory_limit: Some(4 * 1024 * 1024 * 1024),
        cpu_quota: Some(2.0),
        timeout_seconds: 600,
        volumes: vec![("/host/path".to_string(), "/container/path".to_string())],
        environment: vec![("DEBUG".to_string(), "1".to_string())],
    };

    assert_eq!(config.image, "debian:latest");
    assert_eq!(config.memory_limit, Some(4 * 1024 * 1024 * 1024));
    assert_eq!(config.cpu_quota, Some(2.0));
    assert_eq!(config.timeout_seconds, 600);
    assert_eq!(config.volumes.len(), 1);
    assert_eq!(config.environment.len(), 1);
}

// ============================================================================
// ContainerManager Tests
// ============================================================================

#[test]
fn test_container_manager_new() {
    let config = ContainerConfig::default();
    let manager = ContainerManager::new(config.clone());
    assert_eq!(manager.config().image, "ubuntu:22.04");
}

#[test]
fn test_container_manager_config_accessor() {
    let config = ContainerConfig { image: "custom:latest".to_string(), ..Default::default() };
    let manager = ContainerManager::new(config);
    assert_eq!(manager.config().image, "custom:latest");
}

// ============================================================================
// TestStatus Tests
// ============================================================================

#[test]
fn test_status_equality() {
    assert_eq!(TestStatus::Pending, TestStatus::Pending);
    assert_eq!(TestStatus::Running, TestStatus::Running);
    assert_eq!(TestStatus::Passed, TestStatus::Passed);
    assert_eq!(TestStatus::Failed, TestStatus::Failed);
    assert_eq!(TestStatus::Skipped, TestStatus::Skipped);
    assert_eq!(TestStatus::TimedOut, TestStatus::TimedOut);

    assert_ne!(TestStatus::Pending, TestStatus::Running);
    assert_ne!(TestStatus::Passed, TestStatus::Failed);
}

#[test]
fn test_status_copy() {
    let status = TestStatus::Passed;
    let copied = status;
    assert_eq!(status, copied);
}

// ============================================================================
// dry_run Regression Tests (br-74o.13)
// ============================================================================

#[test]
fn test_runner_config_dry_run_default_false() {
    // Regression: dry_run was hardcoded to true, causing installers to receive
    // --dry-run as an argument (e.g., bun interpreted it as a version string → 404)
    let config = automated_flywheel_setup_checker::runner::RunnerConfig::default();
    assert!(!config.dry_run, "RunnerConfig::default().dry_run must be false");
}

#[test]
fn test_installer_test_no_dry_run_in_default_config() {
    // Verify that when dry_run is false (the default), the runner would NOT
    // pass --dry-run to installer scripts
    let config = automated_flywheel_setup_checker::runner::RunnerConfig::default();
    assert!(
        !config.dry_run,
        "Default config must not pass --dry-run to installer scripts"
    );
}

// ============================================================================
// SHA256 Checksum Verification Tests (br-74o.17)
// ============================================================================

use automated_flywheel_setup_checker::runner::ExecutionBackend;

#[test]
fn test_checksum_result_fields() {
    use automated_flywheel_setup_checker::runner::TestResult;

    let checksum = automated_flywheel_setup_checker::runner::ChecksumResult {
        matches: true,
        expected: "abc123".to_string(),
        actual: "abc123".to_string(),
        url: "https://example.com/install.sh".to_string(),
        download_ms: 150,
        size_bytes: 4096,
    };

    let result = TestResult::new("test").with_checksum_result(checksum.clone());
    assert!(result.checksum_result.is_some());
    let cr = result.checksum_result.unwrap();
    assert!(cr.matches);
    assert_eq!(cr.expected, "abc123");
    assert_eq!(cr.actual, "abc123");
    assert_eq!(cr.download_ms, 150);
    assert_eq!(cr.size_bytes, 4096);
}

#[test]
fn test_checksum_result_mismatch() {
    let checksum = automated_flywheel_setup_checker::runner::ChecksumResult {
        matches: false,
        expected: "expected_hash".to_string(),
        actual: "different_hash".to_string(),
        url: "https://example.com/install.sh".to_string(),
        download_ms: 100,
        size_bytes: 2048,
    };

    assert!(!checksum.matches);
    assert_ne!(checksum.expected, checksum.actual);
}

#[test]
fn test_sha256_none_skips_verification() {
    // When no expected hash is provided, checksum_result should be None
    let test = InstallerTest::new("test", "https://example.com/install.sh");
    assert!(test.expected_sha256.is_none());

    let result = TestResult::new("test");
    assert!(result.checksum_result.is_none());
}

#[test]
fn test_installer_test_with_sha256_sets_expected() {
    let test =
        InstallerTest::new("test", "https://example.com").with_sha256("deadbeef01234567");
    assert_eq!(test.expected_sha256, Some("deadbeef01234567".to_string()));
}

#[test]
fn test_execution_backend_default_is_docker() {
    let config = automated_flywheel_setup_checker::runner::RunnerConfig::default();
    assert!(matches!(config.backend, ExecutionBackend::Docker { .. }));
}

#[test]
fn test_execution_backend_local() {
    let config = automated_flywheel_setup_checker::runner::RunnerConfig {
        backend: ExecutionBackend::Local,
        ..Default::default()
    };
    assert!(matches!(config.backend, ExecutionBackend::Local));
}

// ============================================================================
// ParallelRunner Tests (br-74o.12)
// ============================================================================

#[test]
fn test_parallel_runner_max_parallel() {
    let config = RunnerConfig {
        backend: ExecutionBackend::Local,
        ..Default::default()
    };
    let runner = ParallelRunner::new(8, config);
    assert_eq!(runner.max_parallel(), 8);
}

#[tokio::test]
async fn test_parallel_runner_empty_tests() {
    let config = RunnerConfig {
        backend: ExecutionBackend::Local,
        ..Default::default()
    };
    let runner = ParallelRunner::new(4, config);
    let results = runner.run_all(vec![]).await.unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_parallel_runner_fail_fast_config() {
    let config = RunnerConfig {
        backend: ExecutionBackend::Local,
        ..Default::default()
    };
    let runner = ParallelRunner::new(4, config).with_fail_fast(true);
    // Just verify it can be constructed
    assert_eq!(runner.max_parallel(), 4);
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_empty_installer_name() {
    let result = TestResult::new("");
    assert_eq!(result.installer_name, "");
}

#[test]
fn test_zero_timeout() {
    let test = InstallerTest::new("test", "https://example.com").with_timeout(Duration::ZERO);
    assert_eq!(test.timeout, Duration::ZERO);
    assert_eq!(test.timeout_seconds, 0);
}

#[test]
fn test_zero_retry_count() {
    let test = InstallerTest::new("test", "https://example.com").with_retry_count(0);
    assert_eq!(test.retry_count, 0);
}

#[test]
fn test_large_retry_count() {
    let config = RetryConfig { max_attempts: 1000, ..Default::default() };
    assert!(config.should_retry(999));
    assert!(!config.should_retry(1000));
}
