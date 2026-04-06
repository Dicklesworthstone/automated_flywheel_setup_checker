//! Checksums.yaml parsing and validation

mod parser;
mod validator;

pub use parser::{parse_checksums, ChecksumsFile, InstallerEntry};
pub use validator::{check_urls, validate_checksums, UrlCheckResult, ValidationResult};
