//! Test helpers for E2E tests

use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

/// Get the path to the compiled binary
pub fn binary_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target");
    path.push("debug");
    path.push("automated_flywheel_setup_checker");
    path
}

/// Run the checker binary with arguments
pub fn run_checker(args: &[&str]) -> Output {
    let binary = binary_path();
    Command::new(&binary)
        .args(args)
        .output()
        .expect("Failed to execute binary")
}

/// Run checker and capture stdout as string
pub fn run_checker_stdout(args: &[&str]) -> String {
    let output = run_checker(args);
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Run checker and capture stderr as string
pub fn run_checker_stderr(args: &[&str]) -> String {
    let output = run_checker(args);
    String::from_utf8_lossy(&output.stderr).to_string()
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

/// Create a mock config file
pub fn create_mock_config(dir: &std::path::Path, content: &str) -> PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, content).expect("Failed to write config.toml");
    path
}

/// Create a mock error output file
pub fn create_mock_error_output(dir: &std::path::Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("Failed to write error output");
    path
}

/// Check if binary exists (for integration test skip logic)
pub fn binary_exists() -> bool {
    binary_path().exists()
}
