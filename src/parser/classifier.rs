//! Error classification logic
//!
//! Classifies installer failures from their captured output and exit code. Every failed
//! [`TestResult`](crate::runner::TestResult) carries the classification produced here so that
//! persisted results, notifications, and `status --detailed` can report a category instead of
//! "unknown". Patterns are compiled once and cached.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Synthetic marker the executor prepends when an attempt exceeded its timeout.
pub const TIMEOUT_MARKER: &str = "AFSC_TIMEOUT: test timed out";
/// Synthetic marker the executor prepends when a run was cancelled (signal, fail-fast, deadline).
pub const CANCELLED_MARKER: &str = "AFSC_CANCELLED: run cancelled";
/// Synthetic marker for a post-install verification failure (`verify_cmd` / `expect_binary`).
pub const POST_INSTALL_MARKER: &str = "AFSC_POST_INSTALL: verification failed";

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

/// All category names the classifier can produce, in evaluation order.
///
/// Used by documentation drift tests so README tables stay in sync with the code.
pub const CATEGORIES: &[&str] = &[
    "cancelled",
    "timeout",
    "post_install",
    "bootstrap_mismatch",
    "checksum_mismatch",
    "network",
    "apt_repair_failed",
    "command_not_found",
    "permission",
    "dependency",
    "resource",
    "syntax_error",
    "unknown",
];

/// Classify an error based on captured output (stderr plus a stdout tail) and exit code
pub fn classify_error(stderr: &str, exit_code: i32) -> ErrorClassification {
    // Synthetic markers produced by the executor take precedence: they describe what the
    // runner observed, not what the installer printed.
    if stderr.contains(CANCELLED_MARKER) {
        return ErrorClassification {
            severity: ErrorSeverity::Unknown,
            category: "cancelled".to_string(),
            suggestion: Some("Run was cancelled before this installer finished".to_string()),
            retryable: false,
            confidence: 1.0,
        };
    }

    if stderr.contains(TIMEOUT_MARKER) || matches(&TIMEOUT_PATTERNS, stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Transient,
            category: "timeout".to_string(),
            suggestion: Some(
                "Increase the timeout or add an [installers.<name>] timeout_seconds override"
                    .to_string(),
            ),
            retryable: false,
            confidence: 0.95,
        };
    }

    if stderr.contains(POST_INSTALL_MARKER) {
        return ErrorClassification {
            severity: ErrorSeverity::Configuration,
            category: "post_install".to_string(),
            suggestion: Some(
                "Installer exited 0 but post-install verification failed; inspect verify_cmd output"
                    .to_string(),
            ),
            retryable: false,
            confidence: 0.95,
        };
    }

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
            suggestion: Some(
                "Update checksums.yaml with new hash or verify installer integrity".to_string(),
            ),
            retryable: false,
            confidence: 0.95,
        };
    }

    // Network/transient errors
    // A held apt/dpkg lock (unattended-upgrades, another installer) clears itself; retry later.
    if is_apt_lock_held(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Transient,
            category: "dependency".to_string(),
            suggestion: Some("Another apt/dpkg process holds the lock; wait for it (unattended-upgrades) and retry".to_string()),
            retryable: true,
            confidence: 0.9,
        };
    }

    if is_network_error(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Transient,
            category: "network".to_string(),
            suggestion: Some("Check network connectivity and retry".to_string()),
            retryable: true,
            confidence: 0.9,
        };
    }

    // Broken dpkg/apt state needs explicit repair before retrying ACFS.
    if is_apt_repair_error(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Dependency,
            category: "apt_repair_failed".to_string(),
            suggestion: Some("Repair dpkg/apt package state before retrying ACFS".to_string()),
            retryable: false,
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

    // Installers that refuse to run as root print their refusal on stdout or stderr.
    if is_root_refusal(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Permission,
            category: "permission".to_string(),
            suggestion: Some(
                "Installer refuses to run as root; run as a non-root user (the default afsc-base image does)"
                    .to_string(),
            ),
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

    // Prebuilt binaries linked against a newer libc/libstdc++ than the base image ships.
    if is_libc_too_old(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Dependency,
            category: "dependency".to_string(),
            suggestion: Some(
                "The downloaded binary needs a newer glibc/libstdc++ than this base image provides; use a newer base (docker.image, e.g. ubuntu:24.04) or a build for this platform"
                    .to_string(),
            ),
            retryable: false,
            confidence: 0.9,
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
    // SIGKILL (137) inside a container is the OOM killer or a hard cap being enforced.
    if exit_code == 137 || is_oom(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Resource,
            category: "resource".to_string(),
            suggestion: Some(
                "Killed (exit 137 / out of memory): raise [docker].memory_limit or [installers.<name>].memory_limit"
                    .to_string(),
            ),
            retryable: false,
            confidence: 0.85,
        };
    }

    if is_resource_error(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Resource,
            category: "resource".to_string(),
            suggestion: Some("Check available disk space and memory".to_string()),
            retryable: false,
            confidence: 0.75,
        };
    }

    // Syntax errors
    if is_syntax_error(stderr) {
        return ErrorClassification {
            severity: ErrorSeverity::Configuration,
            category: "syntax_error".to_string(),
            suggestion: Some("Fix syntax error in script".to_string()),
            retryable: false,
            confidence: 0.85,
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

/// Explain which pattern (if any) matched, for `classify-error --explain`.
///
/// Returns `(category, pattern, byte offset)` for the first matching pattern in evaluation order.
pub fn explain(stderr: &str, exit_code: i32) -> Option<(&'static str, String, usize)> {
    let groups: &[(&str, &Patterns)] = &[
        ("timeout", &TIMEOUT_PATTERNS),
        ("bootstrap_mismatch", &BOOTSTRAP_PATTERNS),
        ("checksum_mismatch", &CHECKSUM_PATTERNS),
        ("network", &NETWORK_PATTERNS),
        ("apt_repair_failed", &APT_REPAIR_PATTERNS),
        ("permission", &ROOT_REFUSAL_PATTERNS),
        ("permission", &PERMISSION_PATTERNS),
        ("dependency", &DEPENDENCY_PATTERNS),
        ("resource", &RESOURCE_PATTERNS),
        ("syntax_error", &SYNTAX_PATTERNS),
    ];
    for (category, patterns) in groups {
        for re in patterns.get() {
            if let Some(m) = re.find(stderr) {
                return Some((category, re.as_str().to_string(), m.start()));
            }
        }
    }
    if exit_code == 127 {
        return Some(("command_not_found", "exit code 127".to_string(), 0));
    }
    if exit_code == 126 {
        return Some(("permission", "exit code 126".to_string(), 0));
    }
    if exit_code == 137 {
        return Some(("resource", "exit code 137 (SIGKILL / OOM)".to_string(), 0));
    }
    None
}

/// A lazily compiled, cached pattern group.
struct Patterns {
    sources: &'static [&'static str],
    cell: OnceLock<Vec<Regex>>,
}

impl Patterns {
    const fn new(sources: &'static [&'static str]) -> Self {
        Self { sources, cell: OnceLock::new() }
    }

    fn get(&self) -> &Vec<Regex> {
        self.cell.get_or_init(|| self.sources.iter().filter_map(|p| Regex::new(p).ok()).collect())
    }
}

fn matches(patterns: &Patterns, text: &str) -> bool {
    patterns.get().iter().any(|re| re.is_match(text))
}

// Only the executor's own phrasing: curl's "Connection timed out after N ms" is a network error
// and must stay retryable.
static TIMEOUT_PATTERNS: Patterns = Patterns::new(&[r"(?i)\btest timed out after"]);

static SYNTAX_PATTERNS: Patterns =
    Patterns::new(&[r"(?i)syntax error", r"(?i)unexpected token", r"(?i)parse error"]);

static BOOTSTRAP_PATTERNS: Patterns = Patterns::new(&[
    r"(?i)bootstrap.*mismatch",
    r"(?i)bootstrap.*verification.*failed",
    r"(?i)manifest.*mismatch",
    r"(?i)expected.*bootstrap.*actual",
]);

static CHECKSUM_PATTERNS: Patterns = Patterns::new(&[
    r"(?i)checksum.*mismatch",
    r"(?i)checksum.*verification.*failed",
    r"(?i)checksum.*did\s+not\s+match",
    r"(?i)sha256.*mismatch",
    r"(?i)hash.*verification.*failed",
    r"(?i)expected.*hash.*got",
    r"(?i)integrity.*check.*failed",
]);

static NETWORK_PATTERNS: Patterns = Patterns::new(&[
    r"(?i)connection refused",
    r"(?i)connection timed out",
    r"(?i)network unreachable",
    r"(?i)name or service not known",
    r"(?i)temporary failure in name resolution",
    r"(?i)could not resolve host",
    r"(?i)api rate limit exceeded",
    r"(?i)rate limit",
    r"(?i)too many requests",
    r"(?i)\b429\b",
    r"(?i)\b5(00|02|03)\b",
    r"(?i)curl.*failed",
    r"(?i)wget.*failed",
    r"(?i)curl: \(\d+\)",
    r"(?i)ssl_connect|ssl handshake|ssl certificate problem|certificate verify failed",
    r"(?i)connection reset by peer",
    r"(?i)tls handshake",
    r"(?i)unable to fetch some archives",
    r"(?i)could not fetch release info",
    r"(?i)ssl certificate problem",
    r"(?i)unable to acquire.*lock",
    r"(?i)dpkg.*lock",
    r"(?i)apt.*lock",
]);

static APT_REPAIR_PATTERNS: Patterns = Patterns::new(&[
    r"(?i)dpkg\s+--configure\s+-a\s+failed",
    r"(?i)apt-get\s+-f\s+install\s+failed",
    r"(?i)apt\s+repair.*failed",
    r"(?i)dpkg\s+repair.*failed",
    r"(?i)unmet dependencies",
    r"(?i)held broken packages",
    r"(?i)dpkg was interrupted",
    r"(?i)dpkg --configure -a",
    r"(?i)try ['`]?(apt|apt-get)\s+(--fix-broken|-f)\s+install",
    r"(?i)you might want to run ['`]?(apt|apt-get)\s+(--fix-broken|-f)\s+install",
]);

static ROOT_REFUSAL_PATTERNS: Patterns = Patterns::new(&[
    r"(?i)don'?t run (this|the) (script|installer) as root",
    r"(?i)do not run (this|the) (script|installer) as root",
    r"(?i)must not be run as root",
    r"(?i)should not be run as root",
    r"(?i)refus\w* to run as root",
    r"(?i)running as root is not (supported|allowed)",
    r"(?i)please run as a (regular|normal|non-root) user",
]);

static PERMISSION_PATTERNS: Patterns = Patterns::new(&[
    r"(?i)permission denied",
    r"(?i)operation not permitted",
    r"(?i)access denied",
    r"EACCES",
]);

static DEPENDENCY_PATTERNS: Patterns = Patterns::new(&[
    r"(?i)command not found",
    // "minisign is required to verify release authenticity but was not found" (mcp_agent_mail,
    // caam): a verification tool the target host must provide before the installer runs.
    r"(?i)is required (to|for|by) [^
]{0,80}(was )?not (found|installed|available)",
    r"(?i)required tool[^
]{0,40}(missing|not found)",
    r"(?i)package.*not found",
    r"(?i)unable to locate package",
    r"(?i)no such file or directory",
    r"(?i)missing dependency",
]);

static RESOURCE_PATTERNS: Patterns = Patterns::new(&[
    r"(?i)no space left on device",
    r"(?i)out of memory",
    r"(?i)cannot allocate memory",
    r"(?i)disk quota exceeded",
]);

fn is_syntax_error(stderr: &str) -> bool {
    matches(&SYNTAX_PATTERNS, stderr)
}

fn is_bootstrap_mismatch(stderr: &str) -> bool {
    matches(&BOOTSTRAP_PATTERNS, stderr)
}

fn is_checksum_mismatch(stderr: &str) -> bool {
    matches(&CHECKSUM_PATTERNS, stderr)
}

fn is_network_error(stderr: &str) -> bool {
    matches(&NETWORK_PATTERNS, stderr)
}

static LIBC_PATTERNS: Patterns = Patterns::new(&[
    r"GLIBC_\d+\.\d+'? not found",
    r"GLIBCXX_\d+\.\d+(\.\d+)?'? not found",
    r"(?i)version `?GLIBC[A-Z_]*\d",
]);

fn is_libc_too_old(stderr: &str) -> bool {
    matches(&LIBC_PATTERNS, stderr)
}

static APT_LOCK_PATTERNS: Patterns = Patterns::new(&[
    r"(?i)could not get lock /var/lib/(dpkg|apt)",
    r"(?i)unable to acquire the dpkg frontend lock",
    r"(?i)unable to lock the administration directory",
]);

fn is_apt_lock_held(stderr: &str) -> bool {
    matches(&APT_LOCK_PATTERNS, stderr)
}

fn is_apt_repair_error(stderr: &str) -> bool {
    matches(&APT_REPAIR_PATTERNS, stderr)
}

fn is_root_refusal(stderr: &str) -> bool {
    matches(&ROOT_REFUSAL_PATTERNS, stderr)
}

fn is_permission_error(stderr: &str) -> bool {
    matches(&PERMISSION_PATTERNS, stderr)
}

fn is_dependency_error(stderr: &str) -> bool {
    matches(&DEPENDENCY_PATTERNS, stderr)
}

fn is_resource_error(stderr: &str) -> bool {
    matches(&RESOURCE_PATTERNS, stderr)
}

static OOM_PATTERNS: Patterns = Patterns::new(&[
    r"(?i)\bout of memory\b",
    r"(?i)\bOOM[- ]?killed?\b",
    r"(?i)cannot allocate memory",
    r"(?i)memory ?error",
]);

fn is_oom(stderr: &str) -> bool {
    matches(&OOM_PATTERNS, stderr)
}

#[cfg(test)]
mod tests {
    #[test]
    fn sigkill_and_oom_are_resource_failures() {
        let killed = super::classify_error("", 137);
        assert_eq!(killed.category, "resource");
        assert!(!killed.retryable);
        assert!(killed.suggestion.unwrap().contains("memory_limit"));
        let oom = super::classify_error("python3: MemoryError\nKilled", 1);
        assert_eq!(oom.category, "resource");
        let explained = super::explain("", 137).unwrap();
        assert_eq!(explained.0, "resource");
        // A plain non-zero exit with unrelated stderr is still unknown.
        assert_eq!(super::classify_error("something else", 3).category, "unknown");
    }

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

    #[test]
    fn test_classify_timeout_marker() {
        let result = classify_error(&format!("{TIMEOUT_MARKER} after 300s"), -1);
        assert_eq!(result.category, "timeout");
        assert!(!result.retryable);
    }

    #[test]
    fn test_classify_timeout_message() {
        let result = classify_error("Test timed out after 300s", -1);
        assert_eq!(result.category, "timeout");
    }

    #[test]
    fn test_classify_cancelled_marker() {
        let result = classify_error(CANCELLED_MARKER, -1);
        assert_eq!(result.category, "cancelled");
        assert_eq!(result.confidence, 1.0);
    }

    #[test]
    fn test_classify_root_refusal_on_stdout_tail() {
        let text = "SRPS installer\n\u{2717} Don't run this script as root. Run as a regular user with sudo.\n";
        let result = classify_error(text, 1);
        assert_eq!(result.category, "permission");
        assert!(result.suggestion.unwrap().contains("non-root"));
    }

    #[test]
    fn test_categories_list_matches_outputs() {
        for cat in ["timeout", "cancelled", "permission", "network", "dependency", "unknown"] {
            assert!(CATEGORIES.contains(&cat), "{cat} missing from CATEGORIES");
        }
    }

    #[test]
    fn test_explain_reports_pattern_and_offset() {
        let (cat, pattern, offset) =
            explain("prefix\nE: Unable to locate package foo", 100).expect("should match");
        assert_eq!(cat, "dependency");
        assert!(pattern.contains("unable to locate package"));
        assert_eq!(offset, 10);
    }
}
