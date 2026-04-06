//! Tests for the checksums module
//!
//! Tests cover:
//! - YAML parsing with various field combinations
//! - Default value handling
//! - Enabled/disabled filtering
//! - Validation of URLs and checksums
//! - Error handling for malformed input

use automated_flywheel_setup_checker::checksums::{
    parse_checksums, validate_checksums, ChecksumsFile, InstallerEntry, ValidationResult,
};
use std::collections::HashMap;
use std::io::Write;
use tempfile::NamedTempFile;

// ============================================================================
// Parser Tests
// ============================================================================

#[test]
fn test_parse_valid_checksums_yaml() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"
installers:
  zoxide:
    url: "https://raw.githubusercontent.com/ajeetdsouza/zoxide/main/install.sh"
    sha256: "abc123def456789012345678901234567890123456789012345678901234"
    enabled: true
    tags:
      - shell
      - productivity
  rano:
    url: "https://example.com/rano.sh"
    sha256: "def456"
    enabled: true
"#
    )
    .unwrap();

    let checksums = parse_checksums(file.path()).unwrap();
    assert_eq!(checksums.installers.len(), 2);
    assert!(checksums.installers.contains_key("zoxide"));
    assert!(checksums.installers.contains_key("rano"));

    let zoxide = &checksums.installers["zoxide"];
    assert!(zoxide.enabled);
    assert_eq!(zoxide.sha256.as_deref(), Some("abc123def456789012345678901234567890123456789012345678901234"));
    assert_eq!(zoxide.tags.len(), 2);
    assert!(zoxide.tags.contains(&"shell".to_string()));
}

#[test]
fn test_parse_with_defaults() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"
installers:
  minimal:
    url: "https://example.com/install.sh"
"#
    )
    .unwrap();

    let checksums = parse_checksums(file.path()).unwrap();
    let entry = &checksums.installers["minimal"];

    // Check defaults
    assert!(entry.enabled); // default true
    assert!(entry.tags.is_empty()); // default empty
    assert!(entry.version.is_none());
    assert!(entry.sha256.is_none());
}

#[test]
fn test_parse_disabled_installer() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"
installers:
  disabled_tool:
    url: "https://example.com/tool.sh"
    enabled: false
"#
    )
    .unwrap();

    let checksums = parse_checksums(file.path()).unwrap();
    let entry = &checksums.installers["disabled_tool"];
    assert!(!entry.enabled);
}

#[test]
fn test_enabled_installers_filter() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"
installers:
  enabled_one:
    url: "https://a.com"
    enabled: true
  disabled_one:
    url: "https://b.com"
    enabled: false
  enabled_two:
    url: "https://c.com"
    enabled: true
"#
    )
    .unwrap();

    let checksums = parse_checksums(file.path()).unwrap();
    let enabled: Vec<_> = checksums.installers.iter().filter(|(_, e)| e.enabled).collect();
    assert_eq!(enabled.len(), 2);

    let names: Vec<_> = enabled.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"enabled_one"));
    assert!(names.contains(&"enabled_two"));
    assert!(!names.contains(&"disabled_one"));
}

#[test]
fn test_parse_with_sha256() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"
installers:
  tool:
    url: "https://example.com/tool.sh"
    sha256: "deadbeef1234567890abcdef1234567890abcdef1234567890abcdef12345678"
"#
    )
    .unwrap();

    let checksums = parse_checksums(file.path()).unwrap();
    let entry = &checksums.installers["tool"];
    let sha256 = entry.sha256.as_ref().unwrap();
    assert!(sha256.starts_with("deadbeef"));
}

#[test]
fn test_parse_with_extra_fields() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"
installers:
  tool:
    url: "https://example.com/tool.sh"
    custom_field: "custom_value"
    another_field: 42
"#
    )
    .unwrap();

    let checksums = parse_checksums(file.path()).unwrap();
    let entry = &checksums.installers["tool"];
    assert!(entry.extra.contains_key("custom_field"));
}

#[test]
fn test_parse_nonexistent_file() {
    let result = parse_checksums(std::path::Path::new("/nonexistent/path.yaml"));
    assert!(result.is_err());
}

#[test]
fn test_parse_invalid_yaml() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"
this is not: valid: yaml: : :
  - nested
    improperly
"#
    )
    .unwrap();

    let result = parse_checksums(file.path());
    assert!(result.is_err());
}

#[test]
fn test_parse_empty_file() {
    let file = NamedTempFile::new().unwrap();

    let result = parse_checksums(file.path());
    // Empty YAML should parse as empty structure (installers defaults to empty)
    assert!(result.is_ok());
    let checksums = result.unwrap();
    assert!(checksums.installers.is_empty());
}

// ============================================================================
// Validation Tests
// ============================================================================

#[test]
fn test_validate_valid_entry() {
    let mut installers = HashMap::new();
    installers.insert(
        "test".to_string(),
        InstallerEntry {
            url: Some("https://example.com/install.sh".to_string()),
            sha256: Some("abc123".to_string()),
            version: Some("1.0.0".to_string()),
            enabled: true,
            tags: vec![],
            extra: HashMap::new(),
        },
    );

    let checksums = ChecksumsFile { installers };

    let result = validate_checksums(&checksums, false);
    assert!(result.valid);
    assert!(result.errors.is_empty());
}

#[test]
fn test_validate_invalid_url() {
    let mut installers = HashMap::new();
    installers.insert(
        "test".to_string(),
        InstallerEntry {
            url: Some("not-a-valid-url".to_string()),
            sha256: None,
            version: Some("1.0.0".to_string()),
            enabled: true,
            tags: vec![],
            extra: HashMap::new(),
        },
    );

    let checksums = ChecksumsFile { installers };

    let result = validate_checksums(&checksums, false);
    assert!(!result.valid);
    assert_eq!(result.errors.len(), 1);
}

#[test]
fn test_validate_missing_url_disabled() {
    let mut installers = HashMap::new();
    installers.insert(
        "test".to_string(),
        InstallerEntry {
            url: None, // No URL but disabled - should be OK
            sha256: None,
            version: Some("1.0.0".to_string()),
            enabled: false,
            tags: vec![],
            extra: HashMap::new(),
        },
    );

    let checksums = ChecksumsFile { installers };

    let result = validate_checksums(&checksums, false);
    assert!(result.valid); // Disabled entries don't need URLs
}

#[test]
fn test_validate_missing_url_enabled_warns() {
    let mut installers = HashMap::new();
    installers.insert(
        "test".to_string(),
        InstallerEntry {
            url: None,
            sha256: None,
            version: Some("1.0.0".to_string()),
            enabled: true,
            tags: vec![],
            extra: HashMap::new(),
        },
    );

    let checksums = ChecksumsFile { installers };

    let result = validate_checksums(&checksums, false);
    // Should produce a warning, not an error
    assert!(result.valid);
    assert!(!result.warnings.is_empty());
}

#[test]
fn test_validate_missing_sha256_warns() {
    let mut installers = HashMap::new();
    installers.insert(
        "test".to_string(),
        InstallerEntry {
            url: Some("https://example.com/install.sh".to_string()),
            sha256: None,
            version: None,
            enabled: true,
            tags: vec![],
            extra: HashMap::new(),
        },
    );

    let checksums = ChecksumsFile { installers };

    let result = validate_checksums(&checksums, false);
    // Missing sha256 should produce a warning, not an error
    assert!(result.valid);
    assert!(!result.warnings.is_empty());
}

#[test]
fn test_validate_multiple_entries() {
    let mut installers = HashMap::new();
    installers.insert(
        "valid".to_string(),
        InstallerEntry {
            url: Some("https://example.com/valid.sh".to_string()),
            sha256: Some("abc123".to_string()),
            version: Some("1.0.0".to_string()),
            enabled: true,
            tags: vec![],
            extra: HashMap::new(),
        },
    );
    installers.insert(
        "invalid".to_string(),
        InstallerEntry {
            url: Some("not-a-url".to_string()),
            sha256: None,
            version: Some("1.0.0".to_string()),
            enabled: true,
            tags: vec![],
            extra: HashMap::new(),
        },
    );

    let checksums = ChecksumsFile { installers };

    let result = validate_checksums(&checksums, false);
    assert!(!result.valid);
    assert_eq!(result.errors.len(), 1); // Only the invalid entry
}

#[test]
fn test_validation_result_new() {
    let result = ValidationResult::new();
    assert!(result.valid);
    assert!(result.errors.is_empty());
    assert!(result.warnings.is_empty());
}

#[test]
fn test_validation_result_default() {
    let result = ValidationResult::default();
    assert!(result.valid);
}

// ============================================================================
// Installer Entry Tests
// ============================================================================

#[test]
fn test_installer_entry_with_all_fields() {
    let entry = InstallerEntry {
        url: Some("https://example.com/full.sh".to_string()),
        sha256: Some("deadbeef1234".to_string()),
        version: Some("2.0.0".to_string()),
        enabled: true,
        tags: vec!["test".to_string(), "full".to_string()],
        extra: HashMap::new(),
    };

    assert_eq!(entry.version.as_ref().unwrap(), "2.0.0");
    assert_eq!(entry.sha256.as_ref().unwrap(), "deadbeef1234");
    assert_eq!(entry.tags.len(), 2);
}

// ============================================================================
// Integration-style Tests
// ============================================================================

#[test]
fn test_parse_and_validate_fixture() {
    // This tests the full workflow with the sample fixture
    let fixture_path = std::path::Path::new("tests/unit/fixtures/sample_checksums.yaml");
    if fixture_path.exists() {
        let checksums = parse_checksums(fixture_path).unwrap();
        let _result = validate_checksums(&checksums, false);

        // Verify fixture parsed correctly
        assert!(!checksums.installers.is_empty());
    }
}
