//! Validation logic for checksums entries

use super::parser::{ChecksumsFile, InstallerEntry};
use serde::Serialize;
use sha2::{Digest, Sha256};
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

/// URL policy shared by `validate` and `check`: `https://` always; `file://` only when explicitly
/// allowed (tests and local fixtures); plain `http://` and anything else rejected, mirroring
/// ACFS's `enforce_https`. Returns the reason when the URL is not allowed.
pub fn url_policy_violation(url: &str, allow_file_urls: bool) -> Option<String> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("https://") {
        None
    } else if lower.starts_with("file://") {
        if allow_file_urls {
            None
        } else {
            Some(
                "file:// URLs are only allowed with --allow-file-urls or [general].allow_file_urls = true"
                    .to_string(),
            )
        }
    } else if lower.starts_with("http://") {
        Some("plain http:// is not allowed; ACFS enforces https".to_string())
    } else {
        Some("unsupported URL scheme; ACFS enforces https".to_string())
    }
}

/// Apply the URL policy to every enabled installer, returning one error per violation.
pub fn validate_url_policy(
    checksums: &ChecksumsFile,
    allow_file_urls: bool,
) -> Vec<ValidationError> {
    let mut names: Vec<&String> = checksums.installers.keys().collect();
    names.sort();
    let mut errors = Vec::new();
    for name in names {
        let entry = &checksums.installers[name];
        if !entry.enabled {
            continue;
        }
        if let Some(url) = &entry.url {
            if let Some(reason) = url_policy_violation(url, allow_file_urls) {
                errors.push(ValidationError::InvalidUrl(name.clone(), format!("{url}: {reason}")));
            }
        }
    }
    errors
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

/// Result of checking the current URL bytes against the pinned SHA-256.
#[derive(Debug, Serialize)]
pub struct HashCheckResult {
    pub name: String,
    pub url: String,
    pub expected: Option<String>,
    pub actual: Option<String>,
    pub response_time_ms: u64,
    pub matches: bool,
    pub error: Option<String>,
}

fn hash_hex_matches(pinned: &str, downloaded: &str) -> bool {
    let pinned_bytes = pinned.as_bytes();
    let downloaded_bytes = downloaded.as_bytes();

    if pinned_bytes.len() != downloaded_bytes.len() {
        return false;
    }

    pinned_bytes
        .iter()
        .zip(downloaded_bytes)
        .fold(0u8, |diff, (left, right)| diff | (*left ^ *right))
        == 0
}

/// Check all URLs in a checksums file concurrently
///
/// Makes HTTP HEAD requests to each installer URL with a concurrency limit.
/// Redirects are followed because installer URLs are meant to behave like
/// curl-style fetches, where GitHub and vendor-hosted endpoints commonly
/// redirect to a stable download target.
pub async fn check_urls(checksums: &ChecksumsFile) -> Vec<UrlCheckResult> {
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::Semaphore;

    let semaphore = Arc::new(Semaphore::new(10)); // 10 concurrent requests
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(10))
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
            let Ok(_permit) = sem.acquire().await else {
                return UrlCheckResult {
                    name,
                    url,
                    status: None,
                    response_time_ms: 0,
                    reachable: false,
                    error: Some("request semaphore closed".to_string()),
                };
            };
            let start = Instant::now();

            // HEAD first; some hosts reject HEAD (405/403/501) while GET works, so fall back
            // to a ranged GET before declaring a URL broken.
            let mut response = client.head(&url).send().await;
            if let Ok(resp) = &response {
                if matches!(resp.status().as_u16(), 403 | 405 | 501) {
                    response = client.get(&url).header("Range", "bytes=0-0").send().await;
                }
            }
            match response {
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

/// Check all enabled installer URL bytes against their pinned SHA-256 values.
///
/// This is intentionally separate from URL reachability checking: HEAD can
/// prove that an installer endpoint is alive, but only a GET plus hash compare
/// catches the stale-pin regression that breaks ACFS's fail-closed installer.
pub async fn check_hashes(checksums: &ChecksumsFile) -> Vec<HashCheckResult> {
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::Semaphore;

    let semaphore = Arc::new(Semaphore::new(6));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut handles = Vec::new();

    for (name, entry) in &checksums.installers {
        if !entry.enabled {
            continue;
        }

        let url = match &entry.url {
            Some(url) => url.clone(),
            None => continue,
        };
        let expected = entry.sha256.as_ref().map(|hash| hash.trim().to_ascii_lowercase());

        let name = name.clone();
        let client = client.clone();
        let sem = semaphore.clone();

        handles.push(tokio::spawn(async move {
            if expected.as_deref().unwrap_or_default().is_empty() {
                return HashCheckResult {
                    name,
                    url,
                    expected,
                    actual: None,
                    response_time_ms: 0,
                    matches: false,
                    error: Some("missing sha256 checksum".to_string()),
                };
            }

            let Ok(_permit) = sem.acquire().await else {
                return HashCheckResult {
                    name,
                    url,
                    expected,
                    actual: None,
                    response_time_ms: 0,
                    matches: false,
                    error: Some("request semaphore closed".to_string()),
                };
            };
            let start = Instant::now();

            // Local mirrors and test fixtures: hash the file directly (the URL policy decides
            // whether file:// is acceptable; here we only measure).
            if let Some(path) = url.strip_prefix("file://").map(str::to_string) {
                return match std::fs::read(&path) {
                    Ok(bytes) => {
                        let actual = hex::encode(Sha256::digest(&bytes));
                        let matches =
                            expected.as_deref().is_some_and(|e| hash_hex_matches(e, &actual));
                        HashCheckResult {
                            name,
                            url,
                            expected,
                            actual: Some(actual),
                            response_time_ms: start.elapsed().as_millis() as u64,
                            matches,
                            error: None,
                        }
                    }
                    Err(e) => HashCheckResult {
                        name,
                        url,
                        expected,
                        actual: None,
                        response_time_ms: start.elapsed().as_millis() as u64,
                        matches: false,
                        error: Some(format!("cannot read {path}: {e}")),
                    },
                };
            }

            match client.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        return HashCheckResult {
                            name,
                            url,
                            expected,
                            actual: None,
                            response_time_ms: start.elapsed().as_millis() as u64,
                            matches: false,
                            error: Some(format!("HTTP {}", status.as_u16())),
                        };
                    }

                    match resp.bytes().await {
                        Ok(bytes) => {
                            let actual = hex::encode(Sha256::digest(&bytes));
                            let matches = expected
                                .as_deref()
                                .is_some_and(|pinned| hash_hex_matches(pinned, &actual));
                            HashCheckResult {
                                name,
                                url,
                                expected,
                                actual: Some(actual),
                                response_time_ms: start.elapsed().as_millis() as u64,
                                matches,
                                error: if matches {
                                    None
                                } else {
                                    Some("checksum mismatch".to_string())
                                },
                            }
                        }
                        Err(error) => HashCheckResult {
                            name,
                            url,
                            expected,
                            actual: None,
                            response_time_ms: start.elapsed().as_millis() as u64,
                            matches: false,
                            error: Some(error.to_string()),
                        },
                    }
                }
                Err(error) => HashCheckResult {
                    name,
                    url,
                    expected,
                    actual: None,
                    response_time_ms: start.elapsed().as_millis() as u64,
                    matches: false,
                    error: Some(error.to_string()),
                },
            }
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        if let Ok(result) = handle.await {
            results.push(result);
        }
    }

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

    #[tokio::test]
    async fn test_check_urls_falls_back_to_get_when_head_is_rejected() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/no-head.sh"))
            .respond_with(ResponseTemplate::new(405))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/no-head.sh"))
            .respond_with(ResponseTemplate::new(206).set_body_string("#"))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/gone.sh"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/ok.sh"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let mut installers = HashMap::new();
        for (name, p) in [("a_nohead", "/no-head.sh"), ("b_gone", "/gone.sh"), ("c_ok", "/ok.sh")] {
            installers.insert(
                name.to_string(),
                InstallerEntry {
                    url: Some(format!("{}{}", server.uri(), p)),
                    sha256: Some("x".into()),
                    version: None,
                    enabled: true,
                    tags: vec![],
                    extra: HashMap::new(),
                },
            );
        }
        let results = check_urls(&ChecksumsFile { installers }).await;
        let by_name: HashMap<_, _> = results.iter().map(|r| (r.name.as_str(), r)).collect();
        assert!(by_name["a_nohead"].reachable, "GET fallback: {:?}", by_name["a_nohead"]);
        assert_eq!(by_name["a_nohead"].status, Some(206));
        assert!(!by_name["b_gone"].reachable);
        assert!(by_name["c_ok"].reachable);
    }

    #[test]
    fn test_url_policy() {
        assert!(url_policy_violation("https://example.com/i.sh", false).is_none());
        assert!(url_policy_violation("HTTPS://example.com/i.sh", false).is_none());
        assert!(url_policy_violation("file:///tmp/i.sh", true).is_none());
        assert!(url_policy_violation("file:///tmp/i.sh", false)
            .unwrap()
            .contains("allow-file-urls"));
        assert!(url_policy_violation("http://example.com/i.sh", true).unwrap().contains("https"));
        assert!(url_policy_violation("ftp://example.com/i.sh", true).is_some());

        let mut installers = HashMap::new();
        for (name, url, enabled) in [
            ("a_https", "https://x/i.sh", true),
            ("b_http", "http://x/i.sh", true),
            ("c_file", "file:///x/i.sh", true),
            ("d_disabled_http", "http://x/i.sh", false),
        ] {
            installers.insert(
                name.to_string(),
                InstallerEntry {
                    url: Some(url.to_string()),
                    sha256: Some("abc".into()),
                    version: None,
                    enabled,
                    tags: vec![],
                    extra: HashMap::new(),
                },
            );
        }
        let checksums = ChecksumsFile { installers };
        let errors = validate_url_policy(&checksums, false);
        let names: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
        assert_eq!(errors.len(), 2, "{names:?}");
        assert!(names[0].contains("b_http") && names[1].contains("c_file"), "{names:?}");
        assert_eq!(validate_url_policy(&checksums, true).len(), 1, "file allowed");
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
