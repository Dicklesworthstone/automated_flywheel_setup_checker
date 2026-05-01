//! Test helpers for E2E tests

use std::path::PathBuf;
use std::process::Output;
use tempfile::TempDir;

/// Get the path to the compiled binary
pub fn binary_path() -> PathBuf {
    assert_cmd::cargo::cargo_bin!("automated_flywheel_setup_checker").to_path_buf()
}

/// Run the checker binary with arguments
pub fn run_checker(args: &[&str]) -> Output {
    let mut command = assert_cmd::cargo::cargo_bin_cmd!("automated_flywheel_setup_checker");
    command.args(args).output().expect("Failed to execute binary")
}

/// Create a temporary directory for test fixtures
pub fn create_temp_dir() -> TempDir {
    tempfile::tempdir().expect("Failed to create temp dir")
}

/// Create a mock checksums.yaml file
pub fn create_mock_checksums(dir: &std::path::Path, content: &str) -> PathBuf {
    let path = dir.join("checksums.yaml");
    std::fs::write(&path, content).expect("Failed to write checksums.yaml");
    path
}

/// Check if binary exists (for integration test skip logic)
pub fn binary_exists() -> bool {
    binary_path().exists()
}
