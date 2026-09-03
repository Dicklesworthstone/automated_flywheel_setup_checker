//! Secret redaction for captured installer output, notification bodies, and log events.
//!
//! Installers sometimes echo their environment or configuration; captured output is persisted,
//! streamed as JSONL into CI logs, and posted to Slack/GitHub, so credentials must never travel
//! with it. Patterns cover the token shapes that commonly appear on a developer box; each match
//! is replaced by `[redacted:<kind>]`.

use regex::Regex;
use std::sync::OnceLock;

struct Rule {
    kind: &'static str,
    pattern: &'static str,
}

const RULES: &[Rule] = &[
    Rule { kind: "github_token", pattern: r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}\b" },
    Rule { kind: "github_pat", pattern: r"\bgithub_pat_[A-Za-z0-9_]{20,}\b" },
    Rule { kind: "slack_token", pattern: r"\bxox[abpsr]-[A-Za-z0-9-]{10,}\b" },
    Rule { kind: "slack_webhook", pattern: r"https://hooks\.slack\.com/services/[A-Za-z0-9/_-]+" },
    Rule { kind: "aws_access_key", pattern: r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b" },
    Rule { kind: "api_key", pattern: r"\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{16,}\b" },
    Rule {
        kind: "private_key",
        pattern: r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
    },
    Rule {
        kind: "authorization",
        pattern: r"(?i)\b(authorization\s*:\s*(?:bearer|basic|token)\s+)[A-Za-z0-9._~+/=-]{8,}",
    },
    Rule {
        kind: "secret_assignment",
        pattern: r#"(?i)\b((?:[A-Z0-9_]*_)?(?:token|password|passwd|secret|api[_-]?key|access[_-]?key)s?\s*[=:]\s*["']?)([^\s"'&;]{6,})"#,
    },
];

static COMPILED: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();

fn rules() -> &'static [(&'static str, Regex)] {
    COMPILED.get_or_init(|| {
        RULES.iter().filter_map(|r| Regex::new(r.pattern).ok().map(|re| (r.kind, re))).collect()
    })
}

/// Replace secrets in `text` with `[redacted:<kind>]` markers.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for (kind, re) in rules() {
        let marker = format!("[redacted:{kind}]");
        out = match *kind {
            // Keep the key/header name, replace only the value.
            "authorization" | "secret_assignment" => re
                .replace_all(&out, |caps: &regex::Captures| format!("{}{}", &caps[1], marker))
                .to_string(),
            _ => re.replace_all(&out, marker.as_str()).to_string(),
        };
    }
    out
}

/// Whether `text` contains anything the rules would redact.
pub fn contains_secret(text: &str) -> bool {
    rules().iter().any(|(_, re)| re.is_match(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_known_token_shapes_and_keeps_context() {
        let cases = [
            ("token ghp_abcdefghijklmnopqrstuvwxyz0123456789 here", "[redacted:github_token]"),
            ("github_pat_11ABCDEFG0123456789abcdefghijklmnop", "[redacted:github_pat]"),
            ("xoxb-1234567890-abcdefghijk", "[redacted:slack_token]"),
            ("https://hooks.slack.com/services/T000/B000/XXXXXXXX", "[redacted:slack_webhook]"),
            ("AKIAABCDEFGHIJKLMNOP", "[redacted:aws_access_key]"),
            ("sk-ant-api03-abcdefghijklmnopqrstuvwxyz", "[redacted:api_key]"),
            (
                "Authorization: Bearer abcdef.ghijkl.mnopqr",
                "Authorization: Bearer [redacted:authorization]",
            ),
            ("GITHUB_TOKEN=supersecretvalue123", "GITHUB_TOKEN=[redacted:secret_assignment]"),
            ("password: hunter22hunter", "password: [redacted:secret_assignment]"),
        ];
        for (input, expected) in cases {
            let out = redact(input);
            assert!(out.contains(expected), "{input:?} -> {out:?}");
            assert!(contains_secret(input), "{input:?} should be detected");
        }
    }

    #[test]
    fn leaves_ordinary_output_alone() {
        for text in [
            "Installed zoxide to /home/afsc-user/.local/bin",
            "E: Unable to locate package foo",
            "curl: (7) Failed to connect: Connection refused",
            "token bucket refill rate",
            "CHECKSUM_OK 3f715e7ddee3feb9beeb87f094556a80bb3305800f3bcdf9189836a47095f7a0",
        ] {
            assert_eq!(redact(text), text);
            assert!(!contains_secret(text));
        }
    }

    #[test]
    fn redacts_private_key_blocks() {
        let text = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\nBBBB\n-----END OPENSSH PRIVATE KEY-----\nafter";
        let out = redact(text);
        assert_eq!(out, "before\n[redacted:private_key]\nafter");
    }
}
