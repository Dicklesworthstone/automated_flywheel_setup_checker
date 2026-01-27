//! Tests for CLI binary functionality

use super::helpers::*;

#[test]
fn test_binary_help_command() {
    if !binary_exists() {
        eprintln!("SKIP: Binary not built");
        return;
    }

    let output = run_checker(&["--help"]);
    assert!(output.status.success(), "Help command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("automated") || stdout.contains("flywheel") || stdout.contains("checker"),
        "Help should mention the tool name"
    );
}

#[test]
fn test_binary_version_command() {
    if !binary_exists() {
        eprintln!("SKIP: Binary not built");
        return;
    }

    let output = run_checker(&["--version"]);
    assert!(output.status.success(), "Version command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0.") || stdout.contains("1."),
        "Version should contain version number"
    );
}

#[test]
fn test_config_default_command() {
    if !binary_exists() {
        eprintln!("SKIP: Binary not built");
        return;
    }

    let output = run_checker(&["config", "default"]);
    assert!(output.status.success(), "config default should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Config default should output TOML
    assert!(
        stdout.contains("[") || stdout.contains("="),
        "Should output config format"
    );
}

#[test]
fn test_validate_nonexistent_file() {
    if !binary_exists() {
        eprintln!("SKIP: Binary not built");
        return;
    }

    let output = run_checker(&["validate", "--path", "/nonexistent/path/checksums.yaml"]);
    // Should fail gracefully
    assert!(!output.status.success(), "Should fail for nonexistent file");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("No such file") || stderr.contains("error"),
        "Should have error message"
    );
}

#[test]
fn test_validate_valid_checksums() {
    if !binary_exists() {
        eprintln!("SKIP: Binary not built");
        return;
    }

    let temp_dir = create_temp_dir();
    let checksums_content = r#"version: "1.0"

test_tool:
  url: "https://example.com/install.sh"
  checksum:
    algorithm: sha256
    value: "0000000000000000000000000000000000000000000000000000000000000000"
  enabled: true
"#;

    let checksums_path = create_mock_checksums(temp_dir.path(), checksums_content);

    let output = run_checker(&["validate", "--path", checksums_path.to_str().unwrap()]);
    assert!(output.status.success(), "Should validate correct checksums.yaml");
}

#[test]
fn test_cli_unknown_command() {
    if !binary_exists() {
        eprintln!("SKIP: Binary not built");
        return;
    }

    let output = run_checker(&["nonexistent-command"]);
    assert!(!output.status.success(), "Unknown command should fail");
}

#[test]
fn test_cli_no_args_shows_help() {
    if !binary_exists() {
        eprintln!("SKIP: Binary not built");
        return;
    }

    let output = run_checker(&[]);
    // Most CLIs show help with no args
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Usage") || combined.contains("help") || combined.contains("Commands"),
        "No args should show usage info"
    );
}
