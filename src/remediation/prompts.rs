//! Prompt generation for Claude Code remediation

use crate::parser::ErrorClassification;
use std::path::Path;

#[cfg(test)]
use crate::parser::ErrorSeverity;

/// Generate a Claude prompt based on error classification
pub fn generate_prompt(
    classification: &ErrorClassification,
    stderr: &str,
    workspace: &Path,
) -> String {
    let workspace_str = workspace.display();

    match classification.category.as_str() {
        "bootstrap_mismatch" => format!(
            r#"ACFS installer failed with Bootstrap mismatch error:

{stderr}

This means acfs.manifest.yaml was modified but scripts/generated/manifest_index.sh was not regenerated.

Fix this by:
1. Run: bun run generate
2. Verify with: bun run generate --diff
3. If new tools were added, check checksums.yaml has entries for them
4. If checksums.yaml is missing entries, add to KNOWN_INSTALLERS in scripts/lib/security.sh
5. Run: ./scripts/lib/security.sh --update-checksums > checksums.yaml 2>/dev/null
6. Regenerate again: bun run generate
7. Commit all changes with message: "fix: regenerate scripts after manifest change"

Workspace: {workspace_str}

After making changes, verify by running the installer test again."#
        ),

        "checksum_mismatch" => format!(
            r#"ACFS installer failed with Checksum mismatch error:

{stderr}

This means an upstream installer was updated but checksums.yaml has the old hash.

Fix this by:
1. Identify which tool's installer changed from the error message
2. Download the new installer and verify it's legitimate
3. Update checksums.yaml with the new SHA256 hash
4. If this is a new tool, add it to KNOWN_INSTALLERS in scripts/lib/security.sh
5. Run: ./scripts/lib/security.sh --update-checksums > checksums.yaml 2>/dev/null
6. Commit with message: "fix: update <tool> installer checksum"

Workspace: {workspace_str}

IMPORTANT: Only update checksums for legitimate installer changes. Verify the upstream release notes."#
        ),

        "network" => format!(
            r#"ACFS installer failed with a network error:

{stderr}

This is typically a transient issue. Recommended actions:
1. Check network connectivity: ping github.com
2. Check if GitHub/CDN is up: curl -I https://api.github.com
3. If behind proxy, ensure proxy settings are correct
4. Retry the installation

Workspace: {workspace_str}

If the network issue persists, check firewall rules and DNS settings."#
        ),

        "command_not_found" => format!(
            r#"ACFS installer failed because a required command is not installed:

{stderr}

Fix this by:
1. Identify the missing command from the error
2. Install it using apt: sudo apt update && sudo apt install -y <package>
3. Common packages:
   - curl: sudo apt install -y curl
   - jq: sudo apt install -y jq
   - git: sudo apt install -y git
   - bun: curl -fsSL https://bun.sh/install | bash
4. Verify installation: which <command>

Workspace: {workspace_str}"#
        ),

        "dependency" => format!(
            r#"ACFS installer failed due to a missing dependency:

{stderr}

Fix this by:
1. Review the error to identify the missing dependency
2. Check if it's a system package or runtime dependency
3. For system packages: sudo apt update && sudo apt install -y <package>
4. For Python packages: pip install <package>
5. For Node packages: npm install -g <package>

Workspace: {workspace_str}"#
        ),

        "permission" => format!(
            r#"ACFS installer failed due to permission issues:

{stderr}

Fix this by:
1. Check ownership: ls -la <path>
2. Fix ownership if needed: sudo chown -R $USER:$USER <path>
3. Fix permissions: chmod +x <script>
4. For ~/.local: sudo chown -R $USER:$USER ~/.local
5. Avoid running the installer as root unless necessary

Workspace: {workspace_str}

WARNING: Be careful with permission changes. Only modify what's necessary."#
        ),

        "resource" => format!(
            r#"ACFS installer failed due to insufficient system resources:

{stderr}

Fix this by:
1. Check disk space: df -h
2. Check memory: free -h
3. Clean up:
   - Remove old packages: sudo apt autoremove
   - Clear apt cache: sudo apt clean
   - Remove old logs: sudo journalctl --vacuum-time=7d
4. If low on memory, close unnecessary applications

Workspace: {workspace_str}"#
        ),

        _ => generate_generic_prompt(classification, stderr, workspace),
    }
}

fn generate_generic_prompt(
    classification: &ErrorClassification,
    stderr: &str,
    workspace: &Path,
) -> String {
    let suggestion = classification.suggestion.as_deref().unwrap_or("Review the error and fix accordingly");
    let workspace_str = workspace.display();

    format!(
        r#"ACFS installer failed with an error:

Error Category: {}
Severity: {:?}
Retryable: {}

Error Output:
{stderr}

Suggested Fix: {suggestion}

Workspace: {workspace_str}

Please analyze this error and apply the appropriate fix. If you make changes, commit them with a descriptive message."#,
        classification.category,
        classification.severity,
        classification.retryable
    )
}

/// Generate a dry-run report of what the prompt would do
pub fn generate_dry_run_report(
    classification: &ErrorClassification,
    stderr: &str,
    workspace: &Path,
) -> String {
    let prompt = generate_prompt(classification, stderr, workspace);

    format!(
        r#"=== DRY RUN REPORT ===

Would invoke Claude Code with the following prompt:

---
{prompt}
---

Error Classification:
  Category: {}
  Severity: {:?}
  Retryable: {}
  Confidence: {:.0}%

No changes were made (dry run mode)."#,
        classification.category,
        classification.severity,
        classification.retryable,
        classification.confidence * 100.0
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_classification(category: &str, severity: ErrorSeverity) -> ErrorClassification {
        ErrorClassification {
            severity,
            category: category.to_string(),
            suggestion: Some("Test suggestion".to_string()),
            retryable: false,
            confidence: 0.9,
        }
    }

    #[test]
    fn test_bootstrap_mismatch_prompt() {
        let classification = test_classification("bootstrap_mismatch", ErrorSeverity::Configuration);
        let prompt = generate_prompt(
            &classification,
            "Bootstrap mismatch: Expected abc123, Actual def456",
            &PathBuf::from("/data/projects/acfs"),
        );

        assert!(prompt.contains("Bootstrap mismatch"));
        assert!(prompt.contains("bun run generate"));
        assert!(prompt.contains("manifest_index.sh"));
    }

    #[test]
    fn test_checksum_mismatch_prompt() {
        let classification = test_classification("checksum_mismatch", ErrorSeverity::Configuration);
        let prompt = generate_prompt(
            &classification,
            "Checksum verification failed: sha256 mismatch",
            &PathBuf::from("/data/projects/acfs"),
        );

        assert!(prompt.contains("Checksum mismatch"));
        assert!(prompt.contains("checksums.yaml"));
        assert!(prompt.contains("KNOWN_INSTALLERS"));
    }

    #[test]
    fn test_network_error_prompt() {
        let classification = test_classification("network", ErrorSeverity::Transient);
        let prompt = generate_prompt(
            &classification,
            "Connection refused",
            &PathBuf::from("/tmp"),
        );

        assert!(prompt.contains("network"));
        assert!(prompt.contains("transient"));
    }

    #[test]
    fn test_command_not_found_prompt() {
        let classification = test_classification("command_not_found", ErrorSeverity::Dependency);
        let prompt = generate_prompt(
            &classification,
            "bash: jq: command not found",
            &PathBuf::from("/tmp"),
        );

        assert!(prompt.contains("command is not installed"));
        assert!(prompt.contains("apt"));
    }

    #[test]
    fn test_unknown_error_fallback() {
        let classification = test_classification("unknown", ErrorSeverity::Unknown);
        let prompt = generate_prompt(
            &classification,
            "Some unknown error",
            &PathBuf::from("/tmp"),
        );

        assert!(prompt.contains("Error Category: unknown"));
        assert!(prompt.contains("Test suggestion"));
    }

    #[test]
    fn test_dry_run_report() {
        let classification = test_classification("bootstrap_mismatch", ErrorSeverity::Configuration);
        let report = generate_dry_run_report(
            &classification,
            "Bootstrap mismatch error",
            &PathBuf::from("/tmp"),
        );

        assert!(report.contains("DRY RUN REPORT"));
        assert!(report.contains("No changes were made"));
        assert!(report.contains("90%")); // confidence
    }
}
