//! Tests for the error parser and classification module
//!
//! Tests cover:
//! - Classification of various error types (network, permission, dependency, resource)
//! - Confidence scoring
//! - Retryable vs non-retryable errors
//! - Edge cases (empty output, multiple errors)
//! - ParsedError builder pattern

use automated_flywheel_setup_checker::parser::{
    classify_error, ErrorClassification, ErrorSeverity, ParsedError,
};
use std::fs;

// Helper to load fixtures
fn load_error_fixture(name: &str) -> String {
    let path = format!("tests/unit/fixtures/error_outputs/{}", name);
    fs::read_to_string(&path).unwrap_or_else(|_| panic!("Failed to load fixture: {}", path))
}

// ============================================================================
// Network Error Classification Tests
// ============================================================================

#[test]
fn test_classify_dns_resolution_failed() {
    let stderr = "curl: (6) Could not resolve host: example.com";
    let result = classify_error(stderr, 6);
    assert_eq!(result.severity, ErrorSeverity::Transient);
    assert!(result.retryable);
    assert_eq!(result.category, "network");
    assert!(result.confidence >= 0.8);
}

#[test]
fn test_classify_dns_resolution_from_fixture() {
    let stderr = load_error_fixture("network_dns.txt");
    let result = classify_error(&stderr, 6);
    assert_eq!(result.severity, ErrorSeverity::Transient);
    assert!(result.retryable);
}

#[test]
fn test_classify_connection_timeout() {
    let stderr = "curl: (28) Connection timed out after 30001 milliseconds";
    let result = classify_error(stderr, 28);
    assert_eq!(result.severity, ErrorSeverity::Transient);
    assert!(result.retryable);
    assert_eq!(result.category, "network");
}

#[test]
fn test_classify_connection_timeout_from_fixture() {
    let stderr = load_error_fixture("connection_timeout.txt");
    let result = classify_error(&stderr, 28);
    assert_eq!(result.severity, ErrorSeverity::Transient);
}

#[test]
fn test_classify_connection_refused() {
    let stderr = "curl: (7) Failed to connect to localhost port 8080: Connection refused";
    let result = classify_error(stderr, 7);
    assert_eq!(result.severity, ErrorSeverity::Transient);
    assert!(result.retryable);
}

#[test]
fn test_classify_connection_refused_from_fixture() {
    let stderr = load_error_fixture("connection_refused.txt");
    let result = classify_error(&stderr, 7);
    assert_eq!(result.severity, ErrorSeverity::Transient);
}

#[test]
fn test_classify_network_unreachable() {
    let stderr = "Network unreachable";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Transient);
    assert!(result.retryable);
}

#[test]
fn test_classify_name_resolution_failure() {
    let stderr = "Temporary failure in name resolution";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Transient);
}

#[test]
fn test_classify_curl_failed() {
    let stderr = "curl command failed to download file";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Transient);
}

#[test]
fn test_classify_wget_failed() {
    let stderr = "wget download failed";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Transient);
}

// ============================================================================
// Permission Error Classification Tests
// ============================================================================

#[test]
fn test_classify_permission_denied() {
    let stderr = "bash: ./script.sh: Permission denied";
    let result = classify_error(stderr, 126);
    assert_eq!(result.severity, ErrorSeverity::Permission);
    assert!(!result.retryable);
    assert_eq!(result.category, "permission");
}

#[test]
fn test_classify_permission_denied_from_fixture() {
    let stderr = load_error_fixture("permission_denied.txt");
    let result = classify_error(&stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Permission);
}

#[test]
fn test_classify_operation_not_permitted() {
    let stderr = "operation not permitted";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Permission);
    assert!(!result.retryable);
}

#[test]
fn test_classify_access_denied() {
    let stderr = "Access denied to resource";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Permission);
}

#[test]
fn test_classify_eacces() {
    let stderr = "Error: EACCES: permission denied";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Permission);
}

#[test]
fn test_classify_exit_code_126() {
    // Exit code 126 is "Permission problem or command is not executable"
    let stderr = "";
    let result = classify_error(stderr, 126);
    assert_eq!(result.severity, ErrorSeverity::Permission);
}

// ============================================================================
// Dependency Error Classification Tests
// ============================================================================

#[test]
fn test_classify_command_not_found() {
    let stderr = "bash: jq: command not found";
    let result = classify_error(stderr, 127);
    assert_eq!(result.severity, ErrorSeverity::Dependency);
    assert!(!result.retryable);
    // Exit code 127 with "command not found" pattern gives category "command_not_found"
    assert_eq!(result.category, "command_not_found");
    // Confidence varies by implementation
    assert!(result.confidence > 0.0);
}

#[test]
fn test_classify_command_not_found_from_fixture() {
    let stderr = load_error_fixture("command_not_found.txt");
    let result = classify_error(&stderr, 127);
    assert_eq!(result.severity, ErrorSeverity::Dependency);
}

#[test]
fn test_classify_package_not_found() {
    let stderr = "E: Package 'nonexistent-package' not found";
    let result = classify_error(stderr, 100);
    assert_eq!(result.severity, ErrorSeverity::Dependency);
}

#[test]
fn test_classify_unable_to_locate_package() {
    let stderr = "E: Unable to locate package foobar";
    let result = classify_error(stderr, 100);
    assert_eq!(result.severity, ErrorSeverity::Dependency);
}

#[test]
fn test_classify_no_such_file_or_directory() {
    let stderr = "bash: /usr/bin/missing: No such file or directory";
    let result = classify_error(stderr, 127);
    assert_eq!(result.severity, ErrorSeverity::Dependency);
}

#[test]
fn test_classify_missing_dependency() {
    let stderr = "Error: missing dependency libfoo";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Dependency);
}

// ============================================================================
// Resource Error Classification Tests
// ============================================================================

#[test]
fn test_classify_disk_full() {
    let stderr = "No space left on device";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Resource);
    assert!(!result.retryable);
    assert_eq!(result.category, "resource");
}

#[test]
fn test_classify_disk_full_from_fixture() {
    let stderr = load_error_fixture("disk_full.txt");
    let result = classify_error(&stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Resource);
}

#[test]
fn test_classify_out_of_memory() {
    let stderr = "Out of memory";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Resource);
}

#[test]
fn test_classify_cannot_allocate_memory() {
    let stderr = "fork: Cannot allocate memory";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Resource);
}

#[test]
fn test_classify_disk_quota_exceeded() {
    let stderr = "Disk quota exceeded";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Resource);
}

// ============================================================================
// Unknown/Fallback Classification Tests
// ============================================================================

#[test]
fn test_classify_unknown_error() {
    let stderr = "Something completely unexpected happened";
    let result = classify_error(stderr, 255);
    assert_eq!(result.severity, ErrorSeverity::Unknown);
    assert!(!result.retryable);
    assert_eq!(result.confidence, 0.0);
}

#[test]
fn test_classify_empty_stderr() {
    let result = classify_error("", 1);
    // Should classify as unknown with low confidence
    assert_eq!(result.severity, ErrorSeverity::Unknown);
}

#[test]
fn test_classify_whitespace_only() {
    let result = classify_error("   \n\t  ", 1);
    assert_eq!(result.severity, ErrorSeverity::Unknown);
}

// ============================================================================
// Confidence and Priority Tests
// ============================================================================

#[test]
fn test_confidence_exact_match_high() {
    // Exact pattern match should have high confidence
    let stderr = "curl: (6) Could not resolve host: example.com";
    let result = classify_error(stderr, 6);
    assert!(result.confidence >= 0.85, "Confidence should be high for exact match");
}

#[test]
fn test_exit_code_127_high_confidence() {
    // Exit code 127 is definitively command not found
    let stderr = "some generic error";
    let result = classify_error(stderr, 127);
    assert!(result.confidence >= 0.9, "Exit code 127 should have high confidence");
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_classify_case_insensitive() {
    // Patterns should match case-insensitively
    let stderr = "PERMISSION DENIED";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Permission);
}

#[test]
fn test_classify_multiline_stderr() {
    let stderr = "Starting installation...\nDownloading files...\ncurl: (6) Could not resolve host: example.com\nInstallation failed.";
    let result = classify_error(stderr, 6);
    assert_eq!(result.severity, ErrorSeverity::Transient);
}

#[test]
fn test_classify_special_characters() {
    let stderr = "Error: Permission denied for /path/with/special\nchars";
    let result = classify_error(stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Permission);
}

// ============================================================================
// ParsedError Tests
// ============================================================================

#[test]
fn test_parsed_error_new() {
    let error = ParsedError::new("Test error message");
    assert_eq!(error.message, "Test error message");
    assert!(error.exit_code.is_none());
    assert!(error.source_file.is_none());
    assert!(error.line_number.is_none());
    assert!(error.failed_command.is_none());
}

#[test]
fn test_parsed_error_with_exit_code() {
    let error = ParsedError::new("Error").with_exit_code(127);
    assert_eq!(error.exit_code, Some(127));
}

#[test]
fn test_parsed_error_with_source() {
    let error = ParsedError::new("Error").with_source("install.sh", 42);
    assert_eq!(error.source_file, Some("install.sh".to_string()));
    assert_eq!(error.line_number, Some(42));
}

#[test]
fn test_parsed_error_with_command() {
    let error = ParsedError::new("Error").with_command("apt-get install");
    assert_eq!(error.failed_command, Some("apt-get install".to_string()));
}

#[test]
fn test_parsed_error_builder_chain() {
    let error = ParsedError::new("Complex error")
        .with_exit_code(1)
        .with_source("script.sh", 100)
        .with_command("curl http://example.com");

    assert_eq!(error.message, "Complex error");
    assert_eq!(error.exit_code, Some(1));
    assert_eq!(error.source_file, Some("script.sh".to_string()));
    assert_eq!(error.line_number, Some(100));
    assert_eq!(error.failed_command, Some("curl http://example.com".to_string()));
}

// ============================================================================
// Bootstrap and Checksum Error Tests (from fixtures)
// ============================================================================

#[test]
fn test_classify_bootstrap_mismatch() {
    let stderr = load_error_fixture("bootstrap_mismatch.txt");
    let result = classify_error(&stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Configuration);
    assert_eq!(result.category, "bootstrap_mismatch");
    assert!(!result.retryable);
}

#[test]
fn test_classify_checksum_mismatch_from_fixture() {
    let stderr = load_error_fixture("checksum_mismatch.txt");
    let result = classify_error(&stderr, 1);
    assert_eq!(result.severity, ErrorSeverity::Configuration);
    assert_eq!(result.category, "checksum_mismatch");
    assert!(!result.retryable);
}

#[test]
fn test_classify_ssl_certificate_error() {
    let stderr = load_error_fixture("ssl_certificate.txt");
    let result = classify_error(&stderr, 60);
    assert_eq!(result.severity, ErrorSeverity::Transient);
    assert!(result.retryable);
}

#[test]
fn test_classify_apt_lock_error() {
    let stderr = load_error_fixture("apt_lock.txt");
    let result = classify_error(&stderr, 1);
    // Apt lock is a transient issue - another process is holding the lock
    assert_eq!(result.severity, ErrorSeverity::Transient);
    assert!(result.retryable);
}

#[test]
fn test_classify_syntax_error() {
    let stderr = load_error_fixture("syntax_error.txt");
    let result = classify_error(&stderr, 2);
    // Syntax errors are configuration issues
    assert_eq!(result.severity, ErrorSeverity::Configuration);
}

// ============================================================================
// ErrorClassification Tests
// ============================================================================

#[test]
fn test_error_classification_fields() {
    let classification = ErrorClassification {
        severity: ErrorSeverity::Transient,
        category: "network".to_string(),
        suggestion: Some("Retry after checking network".to_string()),
        retryable: true,
        confidence: 0.95,
    };

    assert!(classification.retryable);
    assert!(classification.suggestion.is_some());
    assert_eq!(classification.category, "network");
}

// ============================================================================
// Severity Ordering Tests
// ============================================================================

#[test]
fn test_error_severity_variants() {
    // Ensure all severity variants are distinct
    let severities = [
        ErrorSeverity::Transient,
        ErrorSeverity::Configuration,
        ErrorSeverity::Dependency,
        ErrorSeverity::Permission,
        ErrorSeverity::Resource,
        ErrorSeverity::Unknown,
    ];

    for (i, a) in severities.iter().enumerate() {
        for (j, b) in severities.iter().enumerate() {
            if i == j {
                assert_eq!(a, b);
            } else {
                assert_ne!(a, b);
            }
        }
    }
}
