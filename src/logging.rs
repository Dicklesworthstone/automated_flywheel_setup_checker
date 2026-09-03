//! Tracing/logging setup for the application
//!
//! Logs always go to stderr so that stdout carries only data (`--format json|jsonl` must stay
//! parseable even when installers fail and warnings are emitted). ANSI color is enabled only when
//! stderr is a terminal.

use std::io::IsTerminal;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Log line format for stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable text lines (default).
    #[default]
    Text,
    /// One JSON object per line (for systemd/journald pipelines and log shippers).
    Json,
}

/// Map a verbosity count to a default filter directive.
pub fn default_filter(verbosity: u8) -> &'static str {
    match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    }
}

/// Initialize the tracing subscriber with the given verbosity level (text format).
pub fn init(verbosity: u8) {
    init_with(verbosity, LogFormat::Text, None);
}

/// Initialize the tracing subscriber.
///
/// Precedence for the filter: `RUST_LOG` env var, then `-v` count when non-zero, then the
/// configured `log_level` (when given), then `warn`.
pub fn init_with(verbosity: u8, format: LogFormat, configured_level: Option<&str>) {
    let fallback = if verbosity > 0 {
        default_filter(verbosity).to_string()
    } else {
        configured_level
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| matches!(s.as_str(), "trace" | "debug" | "info" | "warn" | "error"))
            .unwrap_or_else(|| default_filter(0).to_string())
    };

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));

    let ansi = std::io::stderr().is_terminal();

    match format {
        LogFormat::Text => {
            let layer = fmt::layer().with_writer(std::io::stderr).with_ansi(ansi);
            let _ = tracing_subscriber::registry().with(layer).with(env_filter).try_init();
        }
        LogFormat::Json => {
            let layer = fmt::layer().json().with_writer(std::io::stderr).with_ansi(false);
            let _ = tracing_subscriber::registry().with(layer).with(env_filter).try_init();
        }
    }
}
