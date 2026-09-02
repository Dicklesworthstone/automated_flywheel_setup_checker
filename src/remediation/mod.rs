//! Remediation module for auto-fixing failures

pub mod checksums;
mod claude;
mod fallback;
mod outcome;
mod prompts;
mod safety;

pub use claude::{
    get_unavailability_reason, is_claude_available, ChangeType, CircuitState, ClaudeRemediation,
    ClaudeRemediationConfig, FileChange, RateLimitError, RemediationError, RemediationHealth,
    RemediationMethod, RemediationResult, RetryConfig, VerificationResult,
};
pub use claude::{advisory_args, ClaudeEnvelope};
pub use checksums::{
    candidate_path, commit_message, fetch_bytes, plan_refresh, propose, render_candidate,
    verify_entry, ProposalResult, RefreshEntry, RefreshPlan, Verification,
};
pub use fallback::{generate_suggestions, FallbackSuggestion};
pub use outcome::{annotate_risks, RemediationOutcome, RiskNote};
pub use prompts::{generate_dry_run_report, generate_prompt};
pub use safety::{is_command_safe, RiskLevel, SafetyCheck};
