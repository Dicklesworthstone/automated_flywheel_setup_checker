//! Remediation module for auto-fixing failures

mod claude;
mod fallback;
mod prompts;
mod safety;

pub use claude::{
    get_unavailability_reason, is_claude_available, ChangeType, CircuitState, ClaudeRemediation,
    ClaudeRemediationConfig, FileChange, RateLimitError, RemediationError, RemediationHealth,
    RemediationMethod, RemediationResult, RetryConfig, VerificationResult,
};
pub use fallback::FallbackSuggestion;
pub use prompts::{generate_dry_run_report, generate_prompt};
pub use safety::{is_command_safe, SafetyCheck};
