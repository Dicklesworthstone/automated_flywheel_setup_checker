//! Validation logic for checksums entries

use super::parser::{ChecksumsFile, InstallerEntry};
use serde::Serialize;
use thiserror::Error;
use url::Url;

/// Validation error types
#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Missing URL for installer: {0}")]
    MissingUrl(String),
    #[error("Invalid URL for installer {0}: {1}")]
    InvalidUrl(String, String),
    #[error("Missing sha256 checksum for installer: {0}")]
    MissingChecksum(String),
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
    // Check URL — every enabled installer must have one
    if let Some(url) = &entry.url {
        if let Err(e) = Url::parse(url) {
            result.add_error(ValidationError::InvalidUrl(name.to_string(), e.to_string()));
        }
    } else if entry.enabled {
        result.add_warning(format!("No URL specified for enabled installer: {}", name));
    }

    // Check sha256 — every enabled installer should have a checksum
    if entry.sha256.is_none() && entry.enabled {
        result.add_warning(format!("No sha256 checksum for enabled installer: {}", name));
    }
}

/// Result of checking a single URL
#[derive(Debug, Serialize)]
pub struct UrlCheckResult {
    pub name: String,
    pub url: String,
    pub status: Option<u16>,
    pub response_time_ms: u64,
    pub reachable: bool,
    pub error: Option<String>,
}

/// Check all URLs in a checksums file concurrently
///
/// Makes HTTP HEAD requests to each installer URL with a concurrency limit.
pub async fn check_urls(checksums: &ChecksumsFile) -> Vec<UrlCheckResult> {
    use std::time::Instant;
    use tokio::sync::Semaphore;
    use std::sync::Arc;

    let semaphore = Arc::new(Semaphore::new(10)); // 10 concurrent requests
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none()) // Don't follow redirects, report them
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut handles = Vec::new();

    for (name, entry) in &checksums.installers {
        if !entry.enabled {
            continue;
        }
        let url = match &entry.url {
            Some(u) => u.clone(),
            None => continue,
        };

        let name = name.clone();
        let client = client.clone();
        let sem = semaphore.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let start = Instant::now();

            match client.head(&url).send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let elapsed = start.elapsed().as_millis() as u64;
                    let reachable = (200..300).contains(&status);
                    let error = if (300..400).contains(&status) {
                        Some(format!("Redirect ({})", status))
                    } else if status >= 400 {
                        Some(format!("HTTP {}", status))
                    } else {
                        None
                    };

                    UrlCheckResult {
                        name,
                        url,
                        status: Some(status),
                        response_time_ms: elapsed,
                        reachable,
                        error,
                    }
                }
                Err(e) => {
                    let elapsed = start.elapsed().as_millis() as u64;
                    UrlCheckResult {
                        name,
                        url,
                        status: None,
                        response_time_ms: elapsed,
                        reachable: false,
                        error: Some(e.to_string()),
                    }
                }
            }
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        if let Ok(result) = handle.await {
            results.push(result);
        }
    }

    // Sort by name for consistent output
    results.sort_by(|a, b| a.name.cmp(&b.name));
    results
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
