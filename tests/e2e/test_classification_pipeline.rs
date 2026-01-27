//! Tests for the complete error classification pipeline

use automated_flywheel_setup_checker::parser::{classify_error, ErrorSeverity};

#[test]
fn test_pipeline_network_errors_are_transient() {
    let network_errors = vec![
        ("curl: (6) Could not resolve host: example.com", 6),
        ("curl: (28) Connection timed out after 30000 milliseconds", 28),
        ("curl: (7) Failed to connect: Connection refused", 7),
        ("Network unreachable", 1),
        ("Temporary failure in name resolution", 1),
    ];

    for (stderr, exit_code) in network_errors {
        let result = classify_error(stderr, exit_code);
        assert_eq!(
            result.severity,
            ErrorSeverity::Transient,
            "Network error '{}' should be Transient, got {:?}",
            stderr,
            result.severity
        );
        assert!(
            result.retryable,
            "Network error '{}' should be retryable",
            stderr
        );
    }
}

#[test]
fn test_pipeline_permission_errors_are_not_retryable() {
    let permission_errors = vec![
        ("Permission denied", 1),
        ("bash: ./script.sh: Permission denied", 126),
        ("Operation not permitted", 1),
        ("Access denied to resource", 1),
    ];

    for (stderr, exit_code) in permission_errors {
        let result = classify_error(stderr, exit_code);
        assert_eq!(
            result.severity,
            ErrorSeverity::Permission,
            "Permission error '{}' should be Permission, got {:?}",
            stderr,
            result.severity
        );
        assert!(
            !result.retryable,
            "Permission error '{}' should NOT be retryable",
            stderr
        );
    }
}

#[test]
fn test_pipeline_dependency_errors_detected() {
    let dep_errors = vec![
        ("bash: jq: command not found", 127),
        ("E: Unable to locate package nonexistent", 100),
        ("missing dependency libfoo", 1),
    ];

    for (stderr, exit_code) in dep_errors {
        let result = classify_error(stderr, exit_code);
        assert_eq!(
            result.severity,
            ErrorSeverity::Dependency,
            "Dependency error '{}' should be Dependency, got {:?}",
            stderr,
            result.severity
        );
    }
}

#[test]
fn test_pipeline_resource_errors_detected() {
    let resource_errors = vec![
        ("No space left on device", 1),
        ("Out of memory", 1),
        ("Cannot allocate memory", 1),
        ("Disk quota exceeded", 1),
    ];

    for (stderr, exit_code) in resource_errors {
        let result = classify_error(stderr, exit_code);
        assert_eq!(
            result.severity,
            ErrorSeverity::Resource,
            "Resource error '{}' should be Resource, got {:?}",
            stderr,
            result.severity
        );
    }
}

#[test]
fn test_pipeline_exit_code_127_is_definitive() {
    // Exit code 127 means "command not found" in bash
    let result = classify_error("some random output", 127);
    assert_eq!(
        result.severity,
        ErrorSeverity::Dependency,
        "Exit code 127 should always be Dependency"
    );
    assert!(result.confidence >= 0.9, "Exit code 127 should have high confidence");
}

#[test]
fn test_pipeline_exit_code_126_is_permission() {
    // Exit code 126 means "permission denied" or "not executable"
    let result = classify_error("", 126);
    assert_eq!(
        result.severity,
        ErrorSeverity::Permission,
        "Exit code 126 should be Permission"
    );
}

#[test]
fn test_pipeline_bootstrap_mismatch_detected() {
    let stderr = r#"✖ Bootstrap mismatch: generated scripts do not match manifest.
    → Expected: f268d13ef347ba501c480574ec52ca13e0600cf2857921389e1a1d74ad688f0a
    → Actual:   983652176a1ba3a69ce840db1f27d12c1a0f82d52a516d118b172a8ddc87c30e"#;

    let result = classify_error(stderr, 1);
    assert_eq!(
        result.severity,
        ErrorSeverity::Configuration,
        "Bootstrap mismatch should be Configuration"
    );
    assert_eq!(result.category, "bootstrap_mismatch");
    assert!(!result.retryable, "Bootstrap mismatch is not retryable");
}

#[test]
fn test_pipeline_checksum_mismatch_detected() {
    let stderr = "Checksum verification failed: sha256 mismatch";
    let result = classify_error(stderr, 1);
    assert_eq!(
        result.severity,
        ErrorSeverity::Configuration,
        "Checksum mismatch should be Configuration"
    );
    assert_eq!(result.category, "checksum_mismatch");
}

#[test]
fn test_pipeline_unknown_error_low_confidence() {
    let result = classify_error("Something completely unexpected", 255);
    assert_eq!(result.severity, ErrorSeverity::Unknown);
    assert_eq!(result.confidence, 0.0, "Unknown errors should have 0 confidence");
}

#[test]
fn test_pipeline_multiline_error_detection() {
    let stderr = r#"Starting installation...
Downloading files...
curl: (6) Could not resolve host: github.com
Installation failed.
Please check your network connection."#;

    let result = classify_error(stderr, 6);
    assert_eq!(
        result.severity,
        ErrorSeverity::Transient,
        "Should detect network error in multiline output"
    );
}

#[test]
fn test_pipeline_case_insensitive_matching() {
    // Test various case variations
    let variations = vec![
        "PERMISSION DENIED",
        "permission denied",
        "Permission Denied",
        "PerMiSSion DeNied",
    ];

    for stderr in variations {
        let result = classify_error(stderr, 1);
        assert_eq!(
            result.severity,
            ErrorSeverity::Permission,
            "Should match '{}' case-insensitively",
            stderr
        );
    }
}

#[test]
fn test_pipeline_suggestions_provided() {
    // Network error should suggest retry
    let network_result = classify_error("Connection refused", 1);
    assert!(
        network_result.suggestion.is_some(),
        "Network errors should have suggestions"
    );

    // Permission error should suggest checking permissions
    let perm_result = classify_error("Permission denied", 1);
    assert!(
        perm_result.suggestion.is_some(),
        "Permission errors should have suggestions"
    );

    // Unknown errors may not have suggestions
    let unknown_result = classify_error("???", 99);
    // This is OK to not have suggestions for unknown
    assert_eq!(unknown_result.severity, ErrorSeverity::Unknown);
}
