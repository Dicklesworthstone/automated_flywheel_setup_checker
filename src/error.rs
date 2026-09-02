//! Typed top-level errors and the exit-code policy.
//!
//! | code | meaning                                             |
//! |------|-----------------------------------------------------|
//! | 0    | success                                             |
//! | 1    | installer failures or validation drift              |
//! | 2    | usage or configuration error (bad flags, bad config, URL policy) |
//! | 3    | infrastructure error (Docker unreachable, run lock held) |
//! | 4    | validation found drifted checksums or unreachable URLs |
//! | 130  | interrupted by SIGINT                               |
//! | 143  | interrupted by SIGTERM                              |
//!
//! Command handlers return these instead of calling `std::process::exit`, so shutdown
//! notifications (systemd STOPPING) and cleanup always run.

use std::fmt;

/// Signal that interrupted a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Interrupt,
    Terminate,
}

/// Top-level command outcome that maps to a process exit code.
#[derive(Debug)]
pub enum AfscError {
    /// Bad flags or configuration
    Usage(String),
    /// Configuration file problems
    Config(String),
    /// Docker unreachable, locks held, filesystem unavailable
    Infra(String),
    /// Some installers failed (the run itself completed)
    InstallerFailures { failed: usize, total: usize },
    /// `validate` found drift or unreachable URLs
    ValidationDrift(String),
    /// `validate` found format or cross-check errors in checksums.yaml
    ChecksumsInvalid(String),
    /// Interrupted by a signal
    Interrupted(Signal),
    /// Any other error
    Other(anyhow::Error),
}

impl AfscError {
    pub fn exit_code(&self) -> i32 {
        match self {
            AfscError::Usage(_) | AfscError::Config(_) | AfscError::ChecksumsInvalid(_) => 2,
            AfscError::Infra(_) => 3,
            AfscError::InstallerFailures { .. } => 1,
            AfscError::ValidationDrift(_) => 4,
            AfscError::Interrupted(Signal::Interrupt) => 130,
            AfscError::Interrupted(Signal::Terminate) => 143,
            AfscError::Other(_) => 1,
        }
    }

    /// Whether the error should be printed (installer failures are already reported).
    pub fn is_silent(&self) -> bool {
        matches!(
            self,
            AfscError::InstallerFailures { .. }
                | AfscError::ValidationDrift(_)
                | AfscError::ChecksumsInvalid(_)
        )
    }
}

impl fmt::Display for AfscError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AfscError::Usage(m) => write!(f, "{m}"),
            AfscError::Config(m) => write!(f, "{m}"),
            AfscError::Infra(m) => write!(f, "{m}"),
            AfscError::InstallerFailures { failed, total } => {
                write!(f, "{failed} of {total} installers failed")
            }
            AfscError::ValidationDrift(m) => write!(f, "{m}"),
            AfscError::ChecksumsInvalid(m) => write!(f, "{m}"),
            AfscError::Interrupted(Signal::Interrupt) => write!(f, "interrupted (SIGINT)"),
            AfscError::Interrupted(Signal::Terminate) => write!(f, "terminated (SIGTERM)"),
            AfscError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for AfscError {}

impl From<anyhow::Error> for AfscError {
    fn from(e: anyhow::Error) -> Self {
        AfscError::Other(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_policy() {
        assert_eq!(AfscError::Usage("x".into()).exit_code(), 2);
        assert_eq!(AfscError::Config("x".into()).exit_code(), 2);
        assert_eq!(AfscError::Infra("x".into()).exit_code(), 3);
        assert_eq!(AfscError::InstallerFailures { failed: 1, total: 2 }.exit_code(), 1);
        assert_eq!(AfscError::ValidationDrift("x".into()).exit_code(), 4);
        assert_eq!(AfscError::Interrupted(Signal::Interrupt).exit_code(), 130);
        assert_eq!(AfscError::Interrupted(Signal::Terminate).exit_code(), 143);
        assert_eq!(AfscError::Other(anyhow::anyhow!("x")).exit_code(), 1);
        assert!(AfscError::InstallerFailures { failed: 1, total: 1 }.is_silent());
        assert!(!AfscError::Infra("x".into()).is_silent());
    }
}
