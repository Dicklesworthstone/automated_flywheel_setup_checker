//! Scanner for the ACFS repository: `KNOWN_INSTALLERS` and installer call sites.
//!
//! Used by `validate` to cross-check `checksums.yaml` against `scripts/lib/security.sh`
//! (missing entries, extra entries, URL mismatches) and to detect drift between the built-in
//! execution profile table and how ACFS actually invokes each installer
//! (`fetch_and_run_with_runner <runner> <url> <sha> <name> [args]`, with optional `VAR=value`
//! prefixes). Parsing is line-based and intentionally conservative: dynamic call sites
//! (`$tool_q`, `$runner_q`) are ignored.

use super::parser::ChecksumsFile;
use anyhow::{Context, Result};
use regex::Regex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// One literal installer invocation found in the ACFS scripts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallSite {
    pub name: String,
    pub runner: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub file: String,
    pub line: usize,
}

/// Result of scanning an ACFS checkout.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AcfsScan {
    /// `KNOWN_INSTALLERS[name] = url` from `scripts/lib/security.sh`
    pub known_installers: BTreeMap<String, String>,
    pub call_sites: Vec<CallSite>,
    pub scanned_files: Vec<String>,
}

/// Cross-check between `checksums.yaml` and `KNOWN_INSTALLERS`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CrossCheck {
    /// In KNOWN_INSTALLERS but not in checksums.yaml (ACFS would fail closed)
    pub missing_from_checksums: Vec<String>,
    /// In checksums.yaml but not in KNOWN_INSTALLERS (stale entry)
    pub extra_in_checksums: Vec<String>,
    /// (name, checksums.yaml url, KNOWN_INSTALLERS url)
    pub url_mismatches: Vec<(String, String, String)>,
}

impl CrossCheck {
    pub fn has_errors(&self) -> bool {
        !self.missing_from_checksums.is_empty() || !self.url_mismatches.is_empty()
    }
}

/// One difference between a call site and the built-in profile.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileDrift {
    pub name: String,
    pub field: String,
    pub acfs: String,
    pub profile: String,
    pub file: String,
    pub line: usize,
}

fn security_sh(root: &Path) -> PathBuf {
    root.join("scripts").join("lib").join("security.sh")
}

/// Whether the path looks like an ACFS checkout with the security library present.
pub fn is_acfs_repo(root: &Path) -> bool {
    security_sh(root).exists()
}

/// Parse `declare -gA KNOWN_INSTALLERS=( [name]="url" … )`.
pub fn parse_known_installers(text: &str) -> BTreeMap<String, String> {
    static ENTRY: OnceLock<Regex> = OnceLock::new();
    let entry = ENTRY.get_or_init(|| Regex::new(r#"^\s*\[([A-Za-z0-9_.-]+)\]="([^"]+)""#).unwrap());
    let mut out = BTreeMap::new();
    let mut inside = false;
    for line in text.lines() {
        if !inside {
            if line.contains("KNOWN_INSTALLERS=(") {
                inside = true;
            }
            continue;
        }
        if line.trim_start().starts_with(')') {
            break;
        }
        if let Some(c) = entry.captures(line) {
            out.insert(c[1].to_string(), c[2].to_string());
        }
    }
    out
}

/// Parse literal `fetch_and_run_with_runner <runner> <url> <sha> <name> [args]` call sites in
/// one file. Optional `VAR=value` prefixes are captured as env. Dynamic names (`$tool_q`) and
/// dynamic runners are ignored.
pub fn parse_call_sites(text: &str, file: &str) -> Vec<CallSite> {
    static CALL: OnceLock<Regex> = OnceLock::new();
    let call = CALL.get_or_init(|| {
        Regex::new(
            r#"((?:[A-Z_][A-Z0-9_]*=[^\s"']+\s+)*)fetch_and_run_with_runner\s+(\S+)\s+(\S+)\s+(\S+)\s+("?[A-Za-z0-9_.-]+"?|\$\w+)((?:\s+[^\s"';|&)]+)*)"#,
        )
        .unwrap()
    });
    let mut sites = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        for c in call.captures_iter(line) {
            let runner = c[2].to_string();
            if runner != "sh" && runner != "bash" {
                continue; // dynamic runner, or the function's own usage message
            }
            let name = c[5].trim_matches('"').to_string();
            if name.is_empty() || name.starts_with('$') {
                continue;
            }
            let args: Vec<String> = c
                .get(6)
                .map(|m| m.as_str())
                .unwrap_or("")
                .split_whitespace()
                .filter(|t| !t.starts_with('$'))
                .map(|t| t.trim_matches('"').to_string())
                .collect();
            let env: Vec<(String, String)> = c[1]
                .split_whitespace()
                .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
                .collect();
            sites.push(CallSite { name, runner, args, env, file: file.to_string(), line: idx + 1 });
        }
    }
    sites
}

/// Scan an ACFS checkout.
pub fn scan_acfs_repo(root: &Path) -> Result<AcfsScan> {
    let sec = security_sh(root);
    let text = std::fs::read_to_string(&sec)
        .with_context(|| format!("Failed to read {}", sec.display()))?;
    let mut scan = AcfsScan { known_installers: parse_known_installers(&text), ..Default::default() };

    let mut files: Vec<PathBuf> = Vec::new();
    for dir in ["scripts/lib", "scripts/modules"] {
        let d = root.join(dir);
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "sh").unwrap_or(false) {
                    files.push(p);
                }
            }
        }
    }
    let install = root.join("install.sh");
    if install.exists() {
        files.push(install);
    }
    files.sort();

    for path in files {
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
        if let Ok(content) = std::fs::read_to_string(&path) {
            scan.call_sites.extend(parse_call_sites(&content, &rel));
            scan.scanned_files.push(rel);
        }
    }
    scan.call_sites.sort_by(|a, b| a.name.cmp(&b.name).then(a.file.cmp(&b.file)).then(a.line.cmp(&b.line)));
    Ok(scan)
}

/// Compare `checksums.yaml` with `KNOWN_INSTALLERS`.
pub fn cross_check(checksums: &ChecksumsFile, known: &BTreeMap<String, String>) -> CrossCheck {
    let mut cc = CrossCheck::default();
    for (name, url) in known {
        match checksums.installers.get(name) {
            None => cc.missing_from_checksums.push(name.clone()),
            Some(entry) => {
                if let Some(yaml_url) = &entry.url {
                    if yaml_url != url {
                        cc.url_mismatches.push((name.clone(), yaml_url.clone(), url.clone()));
                    }
                }
            }
        }
    }
    for name in checksums.installers.keys() {
        if !known.contains_key(name) {
            cc.extra_in_checksums.push(name.clone());
        }
    }
    cc.extra_in_checksums.sort();
    cc
}

/// Compare literal call sites with the built-in profile table.
pub fn profile_drift(call_sites: &[CallSite]) -> Vec<ProfileDrift> {
    use crate::runner::acfs_profile::profile;
    let mut drift = Vec::new();
    for site in call_sites {
        let p = profile(&site.name);
        if p.interpreter.as_str() != site.runner {
            drift.push(ProfileDrift {
                name: site.name.clone(),
                field: "interpreter".into(),
                acfs: site.runner.clone(),
                profile: p.interpreter.as_str().to_string(),
                file: site.file.clone(),
                line: site.line,
            });
        }
        let pargs: Vec<String> = p.args.iter().map(|s| s.to_string()).collect();
        if pargs != site.args {
            drift.push(ProfileDrift {
                name: site.name.clone(),
                field: "args".into(),
                acfs: site.args.join(" "),
                profile: pargs.join(" "),
                file: site.file.clone(),
                line: site.line,
            });
        }
        let mut penv: Vec<(String, String)> =
            p.env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        penv.sort();
        let mut senv = site.env.clone();
        senv.sort();
        if penv != senv {
            let fmt = |e: &[(String, String)]| {
                e.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ")
            };
            drift.push(ProfileDrift {
                name: site.name.clone(),
                field: "env".into(),
                acfs: fmt(&senv),
                profile: fmt(&penv),
                file: site.file.clone(),
                line: site.line,
            });
        }
    }
    drift
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECURITY: &str = r#"
# comment
declare -gA KNOWN_INSTALLERS=(
    [zoxide]="https://raw.githubusercontent.com/ajeetdsouza/zoxide/main/install.sh"
    [rust]="https://sh.rustup.rs"
    [claude]="https://claude.ai/install.sh"
)
fetch_and_run_with_runner() {
    log_error "fetch_and_run_with_runner requires runner, URL, checksum, and name"
}
fetch_and_run() {
    fetch_and_run_with_runner bash "$url" "$expected_sha256" "$name" "$@"
}
"#;

    const MODULES: &str = r#"
_cli_run_as_user "source $security_lib_q; fetch_and_run_with_runner sh $url_q $expected_sha256_q zoxide" || true
if ! _cli_run_as_user "source $security_lib_q; ATUIN_NO_MODIFY_PATH=1 fetch_and_run_with_runner sh $url_q $expected_sha256_q atuin"; then
fetch_and_run_with_runner sh "$OMZ_INSTALL_URL" "$expected_sha256" "ohmyzsh" --unattended --keep-zshrc
if ! _lang_run_as_user "source $security_lib_q; fetch_and_run_with_runner sh $url_q $expected_sha256_q rust -y"; then
_agent_run_as_user "source $security_lib_q; fetch_and_run_with_runner $runner_q $installer_url_q $installer_sha_q $tool_q"
if _agent_run_as_user "source $security_lib_q; fetch_and_run_with_runner bash $url_q $sha_q claude latest"; then
cmd+="fetch_and_run_with_runner bash $url_q $expected_sha256_q $tool_q"
# fetch_and_run_with_runner sh $url_q $sha_q commented_out
"#;

    #[test]
    fn parses_known_installers_block() {
        let k = parse_known_installers(SECURITY);
        assert_eq!(k.len(), 3);
        assert_eq!(k["rust"], "https://sh.rustup.rs");
    }

    #[test]
    fn parses_literal_call_sites_and_ignores_dynamic_ones() {
        let sites = parse_call_sites(MODULES, "scripts/lib/modules.sh");
        let names: Vec<&str> = sites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["zoxide", "atuin", "ohmyzsh", "rust", "claude"]);
        let atuin = &sites[1];
        assert_eq!(atuin.runner, "sh");
        assert_eq!(atuin.env, vec![("ATUIN_NO_MODIFY_PATH".to_string(), "1".to_string())]);
        let oz = &sites[2];
        assert_eq!(oz.args, vec!["--unattended", "--keep-zshrc"]);
        assert_eq!(sites[3].args, vec!["-y"]);
        assert_eq!(sites[4].runner, "bash");
        assert_eq!(sites[4].args, vec!["latest"]);
        assert_eq!(sites[0].line, 2);
        // The definition and the dynamic wrapper inside security.sh yield nothing.
        assert!(parse_call_sites(SECURITY, "security.sh").is_empty());
    }

    #[test]
    fn built_in_profile_matches_these_call_sites() {
        let sites = parse_call_sites(MODULES, "m.sh");
        let drift = profile_drift(&sites);
        assert!(drift.is_empty(), "{drift:?}");
        // A changed call site is detected.
        let changed = parse_call_sites("fetch_and_run_with_runner bash $u $s rust --quiet\n", "m.sh");
        let drift = profile_drift(&changed);
        let fields: Vec<&str> = drift.iter().map(|d| d.field.as_str()).collect();
        assert!(fields.contains(&"interpreter") && fields.contains(&"args"), "{drift:?}");
    }

    #[test]
    fn cross_check_reports_missing_extra_and_url_mismatch() {
        use crate::checksums::InstallerEntry;
        use std::collections::HashMap;
        let mut installers = HashMap::new();
        let mk = |url: &str| InstallerEntry {
            url: Some(url.to_string()),
            sha256: Some("x".into()),
            version: None,
            enabled: true,
            tags: vec![],
            extra: HashMap::new(),
        };
        installers.insert("zoxide".to_string(), mk("https://raw.githubusercontent.com/ajeetdsouza/zoxide/main/install.sh"));
        installers.insert("rust".to_string(), mk("https://example.com/other"));
        installers.insert("stale".to_string(), mk("https://example.com/stale"));
        let checksums = ChecksumsFile { installers };
        let cc = cross_check(&checksums, &parse_known_installers(SECURITY));
        assert_eq!(cc.missing_from_checksums, vec!["claude"]);
        assert_eq!(cc.extra_in_checksums, vec!["stale"]);
        assert_eq!(cc.url_mismatches.len(), 1);
        assert_eq!(cc.url_mismatches[0].0, "rust");
        assert!(cc.has_errors());
    }

    #[test]
    fn scans_a_synthetic_repo_tree() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("scripts/lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("security.sh"), SECURITY).unwrap();
        std::fs::write(lib.join("cli_tools.sh"), MODULES).unwrap();
        std::fs::write(lib.join("README.txt"), "not a script").unwrap();
        assert!(is_acfs_repo(dir.path()));
        let scan = scan_acfs_repo(dir.path()).unwrap();
        assert_eq!(scan.known_installers.len(), 3);
        assert_eq!(scan.call_sites.len(), 5);
        assert!(scan.scanned_files.iter().any(|f| f.ends_with("cli_tools.sh")));
        assert!(!is_acfs_repo(&dir.path().join("nope")));
    }
}
