//! Validation logic for checksums entries

use super::parser::{ChecksumsFile, InstallerEntry};
use thiserror::Error;
use url::Url;

/// Validation error types
#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Missing URL for installer: {0}")]
    MissingUrl(String),
    #[error("Invalid URL for installer {0}: {1}")]
    InvalidUrl(String, String),
    #[error("Missing version for installer: {0}")]
    MissingVersion(String),
    #[error("HTTP error checking URL {0}: {1}")]
    HttpError(String, String),
}

/// Result of validation
#[derive(Debug)]
pub struct ValidationResult {
    pub valid: bool,
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<String>,
}

impl ValidationResult {
    pub fn new() -> Self {
        Self { valid: true, errors: Vec::new(), warnings: Vec::new() }
    }

    pub fn add_error(&mut self, error: ValidationError) {
        self.valid = false;
        self.errors.push(error);
    }

    pub fn add_warning(&mut self, warning: String) {
        self.warnings.push(warning);
    }
}

impl Default for ValidationResult {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate the structure and content of a checksums file
pub fn validate_checksums(checksums: &ChecksumsFile, check_urls: bool) -> ValidationResult {
    let mut result = ValidationResult::new();

    for (name, entry) in &checksums.installers {
        validate_entry(name, entry, &mut result);
    }

    if check_urls {
        // URL checking would be async in real implementation
        result.add_warning("URL checking not implemented in sync mode".to_string());
    }

    result
}

fn validate_entry(name: &str, entry: &InstallerEntry, result: &mut ValidationResult) {
    // Check URL if present
    if let Some(url) = &entry.url {
        if let Err(e) = Url::parse(url) {
            result.add_error(ValidationError::InvalidUrl(name.to_string(), e.to_string()));
        }
    } else if entry.enabled {
        // Only warn about missing URL for enabled installers
        result.add_warning(format!("No URL specified for enabled installer: {}", name));
    }

    // Check version
    if entry.version.is_none() && entry.enabled {
        result.add_warning(format!("No version specified for enabled installer: {}", name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
}
