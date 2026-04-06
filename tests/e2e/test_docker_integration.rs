//! Docker container lifecycle integration tests (br-74o.7)
//!
//! These tests require a running Docker daemon.
//! They are #[ignore]d by default — run with:
//!   cargo test -- --ignored
//! Or set DOCKER_TESTS=1 environment variable.

use automated_flywheel_setup_checker::runner::{ContainerConfig, ContainerManager};

fn docker_available() -> bool {
    std::env::var("DOCKER_TESTS").is_ok()
}

#[tokio::test]
#[ignore]
async fn test_container_create_and_cleanup() {
    if !docker_available() {
        return;
    }

    let config = ContainerConfig::default();
    let manager = ContainerManager::new(config);

    // Create container
    let container_id = manager.create_container("integration-test").await.unwrap();
    assert!(!container_id.is_empty());

    // Cleanup
    manager.cleanup_container(&container_id).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_exec_simple_command() {
    if !docker_available() {
        return;
    }

    let config = ContainerConfig::default();
    let manager = ContainerManager::new(config);

    let container_id = manager.create_container("exec-test").await.unwrap();

    let (exit_code, stdout, _stderr) = manager
        .exec_in_container(&container_id, &["echo", "hello world"])
        .await
        .unwrap();

    assert_eq!(exit_code, 0);
    assert!(stdout.trim().contains("hello world"));

    manager.cleanup_container(&container_id).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_exec_failing_command() {
    if !docker_available() {
        return;
    }

    let config = ContainerConfig::default();
    let manager = ContainerManager::new(config);

    let container_id = manager.create_container("fail-test").await.unwrap();

    let (exit_code, _stdout, _stderr) = manager
        .exec_in_container(&container_id, &["bash", "-c", "exit 42"])
        .await
        .unwrap();

    assert_eq!(exit_code, 42);

    manager.cleanup_container(&container_id).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_container_environment_vars() {
    if !docker_available() {
        return;
    }

    let config = ContainerConfig {
        environment: vec![("TEST_VAR".to_string(), "test_value".to_string())],
        ..Default::default()
    };
    let manager = ContainerManager::new(config);

    let container_id = manager.create_container("env-test").await.unwrap();

    // Check that our custom env var is set
    let (exit_code, stdout, _stderr) = manager
        .exec_in_container(&container_id, &["bash", "-c", "echo $TEST_VAR"])
        .await
        .unwrap();

    assert_eq!(exit_code, 0);
    assert!(stdout.trim().contains("test_value"));

    // Check non-interactive env vars set by ContainerManager
    let (exit_code, stdout, _stderr) = manager
        .exec_in_container(&container_id, &["bash", "-c", "echo $DEBIAN_FRONTEND"])
        .await
        .unwrap();

    assert_eq!(exit_code, 0);
    assert!(stdout.trim().contains("noninteractive"));

    manager.cleanup_container(&container_id).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_container_cleanup_idempotent() {
    if !docker_available() {
        return;
    }

    let config = ContainerConfig::default();
    let manager = ContainerManager::new(config);

    let container_id = manager.create_container("cleanup-test").await.unwrap();

    // Cleanup twice — second call should not error
    manager.cleanup_container(&container_id).await.unwrap();
    manager.cleanup_container(&container_id).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_container_cleanup_nonexistent() {
    if !docker_available() {
        return;
    }

    let config = ContainerConfig::default();
    let manager = ContainerManager::new(config);

    // Cleanup a nonexistent container — should not error
    manager
        .cleanup_container("nonexistent-container-12345")
        .await
        .unwrap();
}
