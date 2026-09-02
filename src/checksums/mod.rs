//! Checksums.yaml parsing and validation

pub mod acfs;
mod parser;
mod validator;

pub use acfs::{
    cross_check, is_acfs_repo, parse_call_sites, parse_known_installers, profile_drift,
    scan_acfs_repo, AcfsScan, CallSite, CrossCheck, ProfileDrift,
};
pub use parser::{get_enabled_installers, parse_checksums, ChecksumsFile, InstallerEntry};
pub use validator::{
    check_hashes, check_urls, url_policy_violation, validate_checksums, validate_url_policy,
    HashCheckResult, UrlCheckResult, ValidationError, ValidationResult,
};
