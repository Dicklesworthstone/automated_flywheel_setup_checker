//! Honest remediation outcomes. The word "succeeded" is reserved for `Verified` and `Applied`;
//! everything else says exactly what happened (advice given, PR opened, skipped, failed).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RemediationOutcome {
    /// Remediation was not requested for this result
    NotAttempted,
    /// Nothing was tried (mode off, budget exhausted, claude missing, not a remediable failure)
    Skipped { reason: String },
    /// Read-only advice; `risks` lists commands in the advice flagged by the safety checker
    Advised {
        suggestion: String,
        cost_usd: f64,
        #[serde(default)]
        risks: Vec<RiskNote>,
        /// `claude` or `fallback`
        source: String,
    },
    /// A checksum refresh candidate was produced and verified in a fresh container
    Verified {
        installer: String,
        old_sha256: String,
        new_sha256: String,
        candidate_path: String,
        drift_score: Option<String>,
    },
    /// Changes landed on a branch (worktree), with an optional PR
    Proposed {
        branch: String,
        commit: Option<String>,
        pr_url: Option<String>,
        cost_usd: f64,
    },
    /// Branch pushed
    Applied { branch: String, sha: String, pr_url: Option<String>, cost_usd: f64 },
    /// Something went wrong or verification failed
    Failed { reason: String, cost_usd: f64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RiskNote {
    pub command: String,
    pub risk: String,
    pub reason: String,
}

impl RemediationOutcome {
    pub fn kind(&self) -> &'static str {
        match self {
            RemediationOutcome::NotAttempted => "not_attempted",
            RemediationOutcome::Skipped { .. } => "skipped",
            RemediationOutcome::Advised { .. } => "advised",
            RemediationOutcome::Verified { .. } => "verified",
            RemediationOutcome::Proposed { .. } => "proposed",
            RemediationOutcome::Applied { .. } => "applied",
            RemediationOutcome::Failed { .. } => "failed",
        }
    }

    pub fn cost_usd(&self) -> f64 {
        match self {
            RemediationOutcome::Advised { cost_usd, .. }
            | RemediationOutcome::Proposed { cost_usd, .. }
            | RemediationOutcome::Applied { cost_usd, .. }
            | RemediationOutcome::Failed { cost_usd, .. } => *cost_usd,
            _ => 0.0,
        }
    }

    /// Counts toward `remediations_total` (something was actually attempted).
    pub fn attempted(&self) -> bool {
        !matches!(self, RemediationOutcome::NotAttempted | RemediationOutcome::Skipped { .. })
    }

    /// "succeeded" only when a fix was verified or applied.
    pub fn succeeded(&self) -> bool {
        matches!(self, RemediationOutcome::Verified { .. } | RemediationOutcome::Applied { .. })
    }

    /// One-line human wording.
    pub fn describe(&self) -> String {
        match self {
            RemediationOutcome::NotAttempted => "not attempted".into(),
            RemediationOutcome::Skipped { reason } => format!("skipped: {reason}"),
            RemediationOutcome::Advised { cost_usd, risks, source, .. } => {
                let flagged = risks.iter().filter(|r| r.risk == "Critical" || r.risk == "High").count();
                format!(
                    "advice from {source} (${cost_usd:.4}){}",
                    if flagged > 0 { format!(", {flagged} command(s) flagged — NOT applied") } else { ", not applied".into() }
                )
            }
            RemediationOutcome::Verified { new_sha256, candidate_path, drift_score, .. } => format!(
                "checksum refresh verified (new sha {}…, candidate {}{})",
                &new_sha256[..new_sha256.len().min(12)],
                candidate_path,
                drift_score.as_ref().map(|s| format!(", drift {s}")).unwrap_or_default()
            ),
            RemediationOutcome::Proposed { branch, pr_url, cost_usd, .. } => format!(
                "proposed on branch {branch}{} (${cost_usd:.4})",
                pr_url.as_ref().map(|u| format!(", PR {u}")).unwrap_or_default()
            ),
            RemediationOutcome::Applied { branch, sha, cost_usd, .. } => {
                format!("succeeded: pushed {branch} @ {} (${cost_usd:.4})", &sha[..sha.len().min(12)])
            }
            RemediationOutcome::Failed { reason, cost_usd } => format!("failed: {reason} (${cost_usd:.4})"),
        }
    }
}

/// Commands mentioned in an advice text: fenced shell blocks and `$ …` prompt lines. Each is run
/// through the safety checker; only High and Critical come back as notes (sudo alone is Medium).
pub fn annotate_risks(suggestion: &str) -> Vec<RiskNote> {
    use crate::remediation::{is_command_safe, RiskLevel};
    let mut commands: Vec<String> = Vec::new();
    let mut in_fence = false;
    for line in suggestion.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            if !t.is_empty() && !t.starts_with('#') {
                commands.push(t.to_string());
            }
        } else if let Some(cmd) = t.strip_prefix("$ ") {
            commands.push(cmd.to_string());
        } else if let Some(inner) = t.strip_prefix("`$ ").and_then(|r| r.strip_suffix('`')) {
            commands.push(inner.to_string());
        }
    }
    let mut notes = Vec::new();
    for cmd in commands {
        let check = is_command_safe(&cmd);
        if matches!(check.risk_level, RiskLevel::High | RiskLevel::Critical) {
            notes.push(RiskNote {
                command: cmd,
                risk: format!("{:?}", check.risk_level),
                reason: check.reason.unwrap_or_default(),
            });
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risky_commands_in_advice_are_annotated() {
        let dangerous_rm = ["rm", "-rf", "/"].join(" ");
        let dangerous_chmod = ["sudo", "chmod", "-R", "777", "/"].join(" ");
        let advice = format!(
            "Try:\n\n```bash\nsudo apt-get install -y foo\n{dangerous_rm}\n```\n\nor `$ {dangerous_chmod}`\n$ {dangerous_chmod}\n"
        );
        let notes = annotate_risks(&advice);
        assert!(notes.iter().any(|n| n.command == dangerous_rm && n.risk == "Critical"), "{notes:?}");
        assert!(notes.iter().any(|n| n.command == dangerous_chmod), "{notes:?}");
        assert!(!notes.iter().any(|n| n.command == "sudo apt-get install -y foo"), "benign: {notes:?}");
        assert!(annotate_risks("no commands here").is_empty());
    }

    #[test]
    fn round_trips_and_words_outcomes_honestly() {
        let outcomes = vec![
            RemediationOutcome::NotAttempted,
            RemediationOutcome::Skipped { reason: "mode off".into() },
            RemediationOutcome::Advised {
                suggestion: "run apt-get install foo".into(),
                cost_usd: 0.12,
                risks: vec![RiskNote { command: "rm -rf /".into(), risk: "Critical".into(), reason: "recursive delete".into() }],
                source: "claude".into(),
            },
            RemediationOutcome::Verified {
                installer: "uv".into(),
                old_sha256: "a".repeat(64),
                new_sha256: "b".repeat(64),
                candidate_path: "/tmp/c.yaml".into(),
                drift_score: Some("routine".into()),
            },
            RemediationOutcome::Proposed { branch: "afsc/x".into(), commit: Some("abc".into()), pr_url: None, cost_usd: 0.0 },
            RemediationOutcome::Applied { branch: "afsc/x".into(), sha: "abcdef1234567890".into(), pr_url: Some("u".into()), cost_usd: 1.5 },
            RemediationOutcome::Failed { reason: "verification failed".into(), cost_usd: 0.3 },
        ];
        for o in &outcomes {
            let json = serde_json::to_string(o).unwrap();
            let back: RemediationOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, o);
            assert!(json.contains(&format!("\"outcome\":\"{}\"", o.kind())), "{json}");
        }
        assert!(!outcomes[2].succeeded() && outcomes[2].attempted());
        assert!(outcomes[3].succeeded() && outcomes[5].succeeded());
        assert!(!outcomes[1].attempted());
        assert!(outcomes[2].describe().contains("flagged — NOT applied"));
        assert!(outcomes[5].describe().starts_with("succeeded"));
        assert!(!outcomes[4].describe().contains("succeeded"));
        assert_eq!(outcomes[6].cost_usd(), 0.3);
    }
}
