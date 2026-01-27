//! Tests for configuration loading and workflow

use automated_flywheel_setup_checker::config::{load_config, Config};
use std::fs;
use tempfile::TempDir;

fn create_temp_config(content: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("Failed to create temp dir");
    let path = dir.path().join("config.toml");
    fs::write(&path, content).expect("Failed to write config");
    (dir, path)
}

#[test]
fn test_config_default_creation() {
    let config = Config::default();

    // Verify defaults are reasonable
    assert!(config.docker.timeout_seconds > 0, "Timeout should be positive");
    assert!(config.execution.parallel >= 1, "Parallel should be at least 1");
    assert_eq!(config.general.log_level, "info", "Default log level should be info");
}

#[test]
fn test_config_from_toml_file() {
    let toml_content = r#"
[general]
acfs_repo = "/custom/path"
log_level = "debug"

[docker]
image = "ubuntu:24.04"
memory_limit = "4G"
cpu_quota = 2.0
timeout_seconds = 600
pull_policy = "always"

[execution]
parallel = 4
retry_transient = 5
fail_fast = true

[remediation]
enabled = true
auto_commit = false
create_pr = true
max_attempts = 5
"#;

    let (_dir, path) = create_temp_config(toml_content);
    let config = load_config(Some(&path)).expect("Should parse config");

    assert_eq!(config.docker.image, "ubuntu:24.04");
    assert_eq!(config.docker.timeout_seconds, 600);
    assert_eq!(config.execution.parallel, 4);
    assert!(config.remediation.enabled);
}

#[test]
fn test_config_partial_override() {
    // Minimal config - only override docker image
    let toml_content = r#"
[general]
acfs_repo = "/test/path"
log_level = "info"

[docker]
image = "custom:latest"
memory_limit = "2G"
cpu_quota = 1.0
timeout_seconds = 300
pull_policy = "if-not-present"

[execution]
parallel = 1
retry_transient = 3
fail_fast = false

[remediation]
enabled = false
auto_commit = false
create_pr = true
max_attempts = 3
"#;

    let (_dir, path) = create_temp_config(toml_content);
    let config = load_config(Some(&path)).expect("Should parse partial config");

    assert_eq!(config.docker.image, "custom:latest");
}

#[test]
fn test_config_invalid_toml_fails() {
    let invalid_toml = r#"
[general
acfs_repo = not_quoted
"#;

    let (_dir, path) = create_temp_config(invalid_toml);
    let result = load_config(Some(&path));

    assert!(result.is_err(), "Invalid TOML should fail to parse");
}

#[test]
fn test_config_nonexistent_file_fails() {
    let result = load_config(Some(std::path::Path::new("/nonexistent/config.toml")));
    assert!(result.is_err(), "Nonexistent file should fail");
}

#[test]
fn test_config_none_returns_default() {
    let config = load_config(None).expect("None should return default config");
    let default_config = Config::default();

    assert_eq!(config.docker.image, default_config.docker.image);
    assert_eq!(config.execution.parallel, default_config.execution.parallel);
}

#[test]
fn test_config_serialization_roundtrip() {
    let config = Config::default();

    // Serialize to TOML
    let toml_str = toml::to_string(&config).expect("Should serialize");

    // Deserialize back
    let parsed: Config = toml::from_str(&toml_str).expect("Should deserialize");

    // Should match
    assert_eq!(config.docker.image, parsed.docker.image);
    assert_eq!(config.docker.timeout_seconds, parsed.docker.timeout_seconds);
    assert_eq!(config.execution.parallel, parsed.execution.parallel);
}

#[test]
fn test_config_log_levels() {
    let levels = vec!["trace", "debug", "info", "warn", "error"];

    for level in levels {
        let toml_content = format!(
            r#"
[general]
acfs_repo = "/test"
log_level = "{}"

[docker]
image = "ubuntu:22.04"
memory_limit = "2G"
cpu_quota = 1.0
timeout_seconds = 300
pull_policy = "if-not-present"

[execution]
parallel = 1
retry_transient = 3
fail_fast = false

[remediation]
enabled = false
auto_commit = false
create_pr = true
max_attempts = 3
"#,
            level
        );

        let (_dir, path) = create_temp_config(&toml_content);
        let config = load_config(Some(&path)).expect(&format!("Should parse {} level", level));
        assert_eq!(config.general.log_level, level);
    }
}

#[test]
fn test_config_docker_settings() {
    let toml_content = r#"
[general]
acfs_repo = "/test"
log_level = "info"

[docker]
image = "debian:12"
memory_limit = "8G"
cpu_quota = 4.0
timeout_seconds = 900
pull_policy = "always"

[execution]
parallel = 2
retry_transient = 3
fail_fast = false

[remediation]
enabled = false
auto_commit = false
create_pr = true
max_attempts = 3
"#;

    let (_dir, path) = create_temp_config(toml_content);
    let config = load_config(Some(&path)).expect("Should parse docker config");

    assert_eq!(config.docker.image, "debian:12");
    assert_eq!(config.docker.memory_limit, "8G");
    assert_eq!(config.docker.cpu_quota, 4.0);
    assert_eq!(config.docker.timeout_seconds, 900);
    assert_eq!(config.docker.pull_policy, "always");
}

#[test]
fn test_config_execution_settings() {
    let toml_content = r#"
[general]
acfs_repo = "/test"
log_level = "info"

[docker]
image = "ubuntu:22.04"
memory_limit = "2G"
cpu_quota = 1.0
timeout_seconds = 300
pull_policy = "if-not-present"

[execution]
parallel = 8
retry_transient = 10
fail_fast = true

[remediation]
enabled = false
auto_commit = false
create_pr = true
max_attempts = 3
"#;

    let (_dir, path) = create_temp_config(toml_content);
    let config = load_config(Some(&path)).expect("Should parse execution config");

    assert_eq!(config.execution.parallel, 8);
    assert_eq!(config.execution.retry_transient, 10);
    assert!(config.execution.fail_fast);
}

#[test]
fn test_config_remediation_disabled_by_default() {
    let config = Config::default();
    assert!(!config.remediation.enabled, "Remediation should be disabled by default");
}

#[test]
fn test_config_remediation_enabled() {
    let toml_content = r#"
[general]
acfs_repo = "/test"
log_level = "info"

[docker]
image = "ubuntu:22.04"
memory_limit = "2G"
cpu_quota = 1.0
timeout_seconds = 300
pull_policy = "if-not-present"

[execution]
parallel = 1
retry_transient = 3
fail_fast = false

[remediation]
enabled = true
auto_commit = true
create_pr = false
max_attempts = 5
"#;

    let (_dir, path) = create_temp_config(toml_content);
    let config = load_config(Some(&path)).expect("Should parse remediation config");

    assert!(config.remediation.enabled);
    assert!(config.remediation.auto_commit);
    assert!(!config.remediation.create_pr);
    assert_eq!(config.remediation.max_attempts, 5);
}
