//! Error classification logic

use super::error::ParsedError;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Error severity levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorSeverity {
    /// Transient error (network, timeout) - retry may help
    Transient,
    /// Configuration error - user action needed
    Configuration,
    /// Dependency error - missing prerequisite
    Dependency,
    /// Permission error - access denied
    Permission,
    /// Resource error - disk space, memory
    Resource,
    /// Unknown error type
    Unknown,
}

/// Classification result for an error
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorClassification {
    pub severity: ErrorSeverity,
    pub category: String,
    pub suggestion: Option<String>,
    pub retryable: bool,
    pub confidence: f64,
}

/// Classify an error based on stderr content and exit code
pub fn classify_error(stderr: &str, exit_code: i32) -> ErrorClassification {
    // Bootstrap mismatch errors (specific to ACFS installer)
    if is_bootstrap_mismatch(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Configuration,
            category: "bootstrap_mismatch".to_string(),
            suggestion: Some("Regenerate manifest_index.sh to fix bootstrap mismatch".to_string()),
            retryable: false,
            confidence: 0.95,
        };
    }

    // Checksum mismatch errors
    if is_checksum_mismatch(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Configuration,
            category: "checksum_mismatch".to_string(),
            suggestion: Some("Update checksums.yaml with new hash or verify installer integrity".to_string()),
            retryable: false,
            confidence: 0.95,
        };
    }

    // Network/transient errors
    if is_network_error(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Transient,
            category: "network".to_string(),
            suggestion: Some("Check network connectivity and retry".to_string()),
            retryable: true,
            confidence: 0.9,
        };
    }

    // Command not found (exit code 127 is definitive)
    if exit_code == 127 {
        return ErrorClassification {
            severity: ErrorSeverity::Dependency,
            category: "command_not_found".to_string(),
            suggestion: Some("Required command is not installed".to_string()),
            retryable: false,
            confidence: 0.95,
        };
    }

    // Permission errors
    if is_permission_error(stderr)
        || exit_code == 126
        || exit_code == 1 && stderr.contains("Permission denied")
    {
        return ErrorClassification {
            severity: ErrorSeverity::Permission,
            category: "permission".to_string(),
            suggestion: Some("Check file permissions or run with elevated privileges".to_string()),
            retryable: false,
            confidence: 0.85,
        };
    }

    // Dependency errors (general)
    if is_dependency_error(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Dependency,
            category: "dependency".to_string(),
            suggestion: Some("Install missing dependencies".to_string()),
            retryable: false,
            confidence: 0.8,
        };
    }

    // Resource errors
    if is_resource_error(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Resource,
            category: "resource".to_string(),
            suggestion: Some("Check available disk space and memory".to_string()),
            retryable: false,
            confidence: 0.75,
        };
    }

    // Unknown
    ErrorClassification {
        severity: ErrorSeverity::Unknown,
        category: "unknown".to_string(),
        suggestion: None,
        retryable: false,
        confidence: 0.0,
    }
}

fn is_bootstrap_mismatch(stderr: &str) -> bool {
    let patterns = [
        r"(?i)bootstrap.*mismatch",
        r"(?i)bootstrap.*verification.*failed",
        r"(?i)manifest.*mismatch",
        r"(?i)expected.*bootstrap.*actual",
    ];

    patterns
        .iter()
        .any(|p| Regex::new(p).map(|re| re.is_match(stderr)).unwrap_or(false))
}

fn is_checksum_mismatch(stderr: &str) -> bool {
    let patterns = [
        r"(?i)checksum.*mismatch",
        r"(?i)checksum.*verification.*failed",
        r"(?i)sha256.*mismatch",
        r"(?i)hash.*verification.*failed",
        r"(?i)expected.*hash.*got",
        r"(?i)integrity.*check.*failed",
    ];

    patterns
        .iter()
        .any(|p| Regex::new(p).map(|re| re.is_match(stderr)).unwrap_or(false))
}

fn is_network_error(stderr: &str) -> bool {
    let patterns = [
        r"(?i)connection refused",
        r"(?i)connection timed out",
        r"(?i)network unreachable",
        r"(?i)name or service not known",
        r"(?i)temporary failure in name resolution",
        r"(?i)could not resolve host",
        r"(?i)curl.*failed",
        r"(?i)wget.*failed",
    ];

    patterns
        .iter()
        .any(|p| Regex::new(p).map(|re| re.is_match(stderr)).unwrap_or(false))
}

fn is_permission_error(stderr: &str) -> bool {
    let patterns = [
        r"(?i)permission denied",
        r"(?i)operation not permitted",
        r"(?i)access denied",
        r"(?i)EACCES",
    ];

    patterns.iter().any(|p| Regex::new(p).map(|re| re.is_match(stderr)).unwrap_or(false))
}

fn is_dependency_error(stderr: &str) -> bool {
    let patterns = [
        r"(?i)command not found",
        r"(?i)package.*not found",
        r"(?i)unable to locate package",
        r"(?i)no such file or directory",
        r"(?i)missing dependency",
    ];

    patterns.iter().any(|p| Regex::new(p).map(|re| re.is_match(stderr)).unwrap_or(false))
}

fn is_resource_error(stderr: &str) -> bool {
    let patterns = [
        r"(?i)no space left on device",
        r"(?i)out of memory",
        r"(?i)cannot allocate memory",
        r"(?i)disk quota exceeded",
    ];

    patterns.iter().any(|p| Regex::new(p).map(|re| re.is_match(stderr)).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_bootstrap_mismatch() {
        let result = classify_error("Bootstrap mismatch: Expected abc123, Actual def456", 1);
        assert_eq!(result.severity, ErrorSeverity::Configuration);
        assert_eq!(result.category, "bootstrap_mismatch");
        assert!(!result.retryable);
    }

    #[test]
    fn test_classify_checksum_mismatch() {
        let result = classify_error("Checksum verification failed: sha256 mismatch", 1);
        assert_eq!(result.severity, ErrorSeverity::Configuration);
        assert_eq!(result.category, "checksum_mismatch");
        assert!(!result.retryable);
    }

    #[test]
    fn test_classify_network_error() {
        let result = classify_error("curl: (7) Failed to connect: Connection refused", 7);
        assert_eq!(result.severity, ErrorSeverity::Transient);
        assert!(result.retryable);
    }

    #[test]
    fn test_classify_permission_error() {
        let result = classify_error("bash: ./script.sh: Permission denied", 126);
        assert_eq!(result.severity, ErrorSeverity::Permission);
        assert!(!result.retryable);
    }

    #[test]
    fn test_classify_command_not_found() {
        let result = classify_error("bash: jq: command not found", 127);
        assert_eq!(result.severity, ErrorSeverity::Dependency);
    }
}
