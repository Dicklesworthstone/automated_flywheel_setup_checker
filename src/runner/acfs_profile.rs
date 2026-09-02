//! Built-in ACFS execution profiles.
//!
//! ACFS runs each verified installer as `<runner> <staged-file> <args...>` from
//! `scripts/lib/security.sh::fetch_and_run_with_runner`, and individual modules choose the runner
//! and arguments (for example `zsh.sh` runs oh-my-zsh with `sh … --unattended --keep-zshrc`).
//! This table mirrors those call sites so the checker executes installers the way ACFS does. A
//! drift test (`validate --profile`, `tests/acfs_profile_drift.rs`) compares it against the ACFS
//! checkout. `[installers.<name>]` overrides in the config take precedence over this table.

/// Shell interpreter used to run an installer script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interpreter {
    Bash,
    Sh,
}

impl Interpreter {
    pub fn as_str(&self) -> &'static str {
        match self {
            Interpreter::Bash => "bash",
            Interpreter::Sh => "sh",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "bash" => Some(Interpreter::Bash),
            "sh" => Some(Interpreter::Sh),
            _ => None,
        }
    }
}

/// Execution profile for one installer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub interpreter: Interpreter,
    pub args: &'static [&'static str],
    pub env: &'static [(&'static str, &'static str)],
    /// Minimum timeout in seconds (heavy installers)
    pub min_timeout_seconds: Option<u64>,
    /// Command whose first output line is recorded as `installed_version`
    pub version_cmd: Option<&'static str>,
    /// Why this entry exists (ACFS call site)
    pub source: &'static str,
}

const DEFAULT_PROFILE: Profile = Profile {
    interpreter: Interpreter::Bash,
    args: &[],
    env: &[],
    min_timeout_seconds: None,
    version_cmd: None,
    source: "default: scripts/lib/stack.sh generic path (bash, no args)",
};

/// Built-in profile table. Keep in sync with ACFS call sites (the drift test enforces it).
pub const TABLE: &[(&str, Profile)] = &[
    (
        "zoxide",
        Profile {
            interpreter: Interpreter::Sh,
            args: &[],
            env: &[],
            min_timeout_seconds: None,
            version_cmd: Some("zoxide --version"),
            source: "scripts/lib/cli_tools.sh: fetch_and_run_with_runner sh … zoxide",
        },
    ),
    (
        "atuin",
        Profile {
            interpreter: Interpreter::Sh,
            args: &[],
            env: &[("ATUIN_NO_MODIFY_PATH", "1")],
            min_timeout_seconds: None,
            version_cmd: Some("atuin --version"),
            source: "scripts/lib/cli_tools.sh: ATUIN_NO_MODIFY_PATH=1 fetch_and_run_with_runner sh … atuin",
        },
    ),
    (
        "uv",
        Profile {
            interpreter: Interpreter::Sh,
            args: &[],
            env: &[],
            min_timeout_seconds: None,
            version_cmd: Some("uv --version"),
            source: "scripts/lib/languages.sh: fetch_and_run_with_runner sh … uv",
        },
    ),
    (
        "rust",
        Profile {
            interpreter: Interpreter::Sh,
            args: &["-y"],
            env: &[],
            min_timeout_seconds: Some(600),
            version_cmd: Some("cargo --version"),
            source: "scripts/lib/languages.sh: fetch_and_run_with_runner sh … rust -y",
        },
    ),
    (
        "ubs",
        Profile {
            interpreter: Interpreter::Bash,
            args: &[],
            env: &[],
            // The installer falls back to `cargo install ast-grep` (a full Rust build) when no
            // prebuilt ast-grep is present: 300 s is not enough in a fresh container.
            min_timeout_seconds: Some(900),
            version_cmd: Some("ubs --version"),
            source: "scripts/lib/stack.sh: install_ubs (dynamic runner; cargo install ast-grep fallback)",
        },
    ),
    (
        "bun",
        Profile {
            interpreter: Interpreter::Bash,
            args: &[],
            env: &[],
            min_timeout_seconds: None,
            version_cmd: Some("bun --version"),
            source: "scripts/lib/languages.sh: fetch_and_run_with_runner bash … bun",
        },
    ),
    (
        "ohmyzsh",
        Profile {
            interpreter: Interpreter::Sh,
            args: &["--unattended", "--keep-zshrc"],
            env: &[],
            min_timeout_seconds: None,
            version_cmd: None,
            source: "scripts/lib/zsh.sh: fetch_and_run_with_runner sh … ohmyzsh --unattended --keep-zshrc",
        },
    ),
    (
        "claude",
        Profile {
            interpreter: Interpreter::Bash,
            args: &["latest"],
            env: &[],
            min_timeout_seconds: None,
            version_cmd: Some("claude --version"),
            source: "scripts/lib/agents.sh: fetch_and_run_with_runner bash … claude latest",
        },
    ),
    (
        "gemini_patch",
        Profile {
            interpreter: Interpreter::Bash,
            args: &[],
            env: &[],
            min_timeout_seconds: None,
            version_cmd: None,
            source: "scripts/lib/agents.sh: patch script run with bash; needs node on PATH",
        },
    ),
    (
        "mdwb",
        Profile {
            interpreter: Interpreter::Bash,
            args: &[],
            env: &[],
            min_timeout_seconds: Some(900),
            version_cmd: None,
            source: "heavy build (Rust + Chromium deps); 900 s floor observed",
        },
    ),
    (
        "nvm",
        Profile {
            interpreter: Interpreter::Bash,
            args: &[],
            env: &[],
            min_timeout_seconds: None,
            version_cmd: Some("bash -lc 'source ~/.nvm/nvm.sh >/dev/null 2>&1; nvm --version'"),
            source: "nvm install script (bash)",
        },
    ),
];

/// Look up the built-in profile for an installer (default: bash, no args).
pub fn profile(name: &str) -> &'static Profile {
    TABLE.iter().find(|(n, _)| *n == name).map(|(_, p)| p).unwrap_or(&DEFAULT_PROFILE)
}

/// Whether the table has an explicit entry for `name`.
pub fn has_profile(name: &str) -> bool {
    TABLE.iter().any(|(n, _)| *n == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_profiles_match_acfs_call_sites() {
        let oz = profile("ohmyzsh");
        assert_eq!(oz.interpreter, Interpreter::Sh);
        assert_eq!(oz.args, &["--unattended", "--keep-zshrc"]);
        let atuin = profile("atuin");
        assert_eq!(atuin.env, &[("ATUIN_NO_MODIFY_PATH", "1")]);
        assert_eq!(profile("rust").args, &["-y"]);
        assert_eq!(profile("claude").args, &["latest"]);
        assert_eq!(profile("claude").interpreter, Interpreter::Bash);
    }

    #[test]
    fn unknown_installer_gets_the_default_profile() {
        let p = profile("something_new");
        assert_eq!(p.interpreter, Interpreter::Bash);
        assert!(p.args.is_empty());
        assert!(!has_profile("something_new"));
        assert!(has_profile("zoxide"));
    }

    #[test]
    fn interpreter_parses() {
        assert_eq!(Interpreter::parse("sh"), Some(Interpreter::Sh));
        assert_eq!(Interpreter::parse(" bash "), Some(Interpreter::Bash));
        assert_eq!(Interpreter::parse("zsh"), None);
    }
}
