//! Resolved installer specification.
//!
//! One [`InstallerSpec`] per installer combines the `checksums.yaml` entry, the built-in ACFS
//! execution profile, the `[installers.<name>]` config override, and the global settings.
//! Precedence per field: override > profile > global. `--dry-run` prints the resolved spec so an
//! operator can see exactly what will run.

use super::acfs_profile::{self, Interpreter};
use super::installer::InstallerTest;
use crate::checksums::InstallerEntry;
use crate::config::InstallerOverride;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

/// Where a resolved field came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldSource {
    Global,
    Profile,
    Override,
}

/// Fully resolved installer specification.
#[derive(Debug, Clone, Serialize)]
pub struct InstallerSpec {
    pub name: String,
    pub url: String,
    pub sha256: Option<String>,
    pub interpreter: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub timeout_seconds: u64,
    /// Retries after the first attempt (attempts = retries + 1)
    pub retries: u32,
    pub skip_reason: Option<String>,
    pub expect_binary: Option<String>,
    pub verify_cmd: Option<String>,
    pub version_cmd: Option<String>,
    pub run_as_root: Option<bool>,
    pub memory_limit: Option<String>,
    pub network: Option<String>,
    /// Source of each notable field
    pub sources: BTreeMap<String, FieldSource>,
}

/// Global inputs to spec resolution.
#[derive(Debug, Clone, Copy)]
pub struct GlobalDefaults {
    pub timeout_seconds: u64,
    pub retries: u32,
}

/// Resolve a spec for one installer.
pub fn resolve_spec(
    name: &str,
    entry: &InstallerEntry,
    ovr: Option<&InstallerOverride>,
    global: GlobalDefaults,
) -> InstallerSpec {
    let profile = acfs_profile::profile(name);
    let has_profile = acfs_profile::has_profile(name);
    let mut sources = BTreeMap::new();

    // interpreter
    let (interpreter, src) = match ovr.and_then(|o| o.interpreter.as_deref()) {
        Some(s) => (
            Interpreter::parse(s)
                .map(|i| i.as_str().to_string())
                .unwrap_or_else(|| s.trim().to_string()),
            FieldSource::Override,
        ),
        None if has_profile => (profile.interpreter.as_str().to_string(), FieldSource::Profile),
        None => (profile.interpreter.as_str().to_string(), FieldSource::Global),
    };
    sources.insert("interpreter".into(), src);

    // args
    let (args, src) = match ovr.and_then(|o| o.args.clone()) {
        Some(a) => (a, FieldSource::Override),
        None if !profile.args.is_empty() => {
            (profile.args.iter().map(|s| s.to_string()).collect(), FieldSource::Profile)
        }
        None => (Vec::new(), FieldSource::Global),
    };
    sources.insert("args".into(), src);

    // env: profile then override (override wins per key)
    let mut env: BTreeMap<String, String> =
        profile.env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let mut env_src = if env.is_empty() { FieldSource::Global } else { FieldSource::Profile };
    if let Some(o) = ovr {
        if !o.env.is_empty() {
            env_src = FieldSource::Override;
            for (k, v) in &o.env {
                env.insert(k.clone(), v.clone());
            }
        }
    }
    sources.insert("env".into(), env_src);

    // timeout: override > max(global, profile floor)
    let (timeout_seconds, src) = match ovr.and_then(|o| o.timeout_seconds) {
        Some(t) => (t, FieldSource::Override),
        None => match profile.min_timeout_seconds {
            Some(floor) if floor > global.timeout_seconds => (floor, FieldSource::Profile),
            _ => (global.timeout_seconds, FieldSource::Global),
        },
    };
    sources.insert("timeout_seconds".into(), src);

    // retries
    let (retries, src) = match ovr.and_then(|o| o.retry) {
        Some(r) => (r, FieldSource::Override),
        None => (global.retries, FieldSource::Global),
    };
    sources.insert("retries".into(), src);

    let skip_reason = match ovr {
        Some(o) if o.skip == Some(true) => Some(
            o.skip_reason.clone().unwrap_or_else(|| "skipped by [installers] override".to_string()),
        ),
        _ => None,
    };

    let version_cmd = ovr
        .and_then(|o| o.version_cmd.clone())
        .or_else(|| profile.version_cmd.map(|s| s.to_string()));

    InstallerSpec {
        name: name.to_string(),
        url: entry.url.clone().unwrap_or_default(),
        sha256: entry.sha256.clone(),
        interpreter,
        args,
        env,
        timeout_seconds,
        retries,
        skip_reason,
        expect_binary: ovr.and_then(|o| o.expect_binary.clone()),
        verify_cmd: ovr.and_then(|o| o.verify_cmd.clone()),
        version_cmd,
        run_as_root: ovr.and_then(|o| o.run_as_root),
        memory_limit: ovr.and_then(|o| o.memory_limit.clone()),
        network: ovr.and_then(|o| o.network.clone()),
        sources,
    }
}

impl InstallerSpec {
    /// Build the executor input from this spec.
    pub fn to_test(&self) -> InstallerTest {
        let mut test = InstallerTest::new(&self.name, &self.url)
            .with_timeout(Duration::from_secs(self.timeout_seconds))
            .with_retry_count(self.retries.saturating_add(1).max(1))
            .with_interpreter(&self.interpreter)
            .with_args(self.args.clone());
        if let Some(sha) = &self.sha256 {
            test = test.with_sha256(sha);
        }
        for (k, v) in &self.env {
            test = test.with_env(k, v);
        }
        if let Some(b) = &self.expect_binary {
            test = test.with_expect_binary(b);
        }
        if let Some(c) = &self.verify_cmd {
            test = test.with_verify_cmd(c);
        }
        if let Some(c) = &self.version_cmd {
            test = test.with_version_cmd(c);
        }
        if let Some(bytes) =
            self.memory_limit.as_deref().and_then(super::container::parse_memory_limit)
        {
            test = test.with_memory_limit(bytes);
        }
        if let Some(net) = &self.network {
            test = test.with_network(net);
        }
        if let Some(root) = self.run_as_root {
            test = test.with_run_as_root(root);
        }
        test
    }

    /// Short human rendering of the command that will run.
    pub fn command_line(&self) -> String {
        let mut s = format!("{} /tmp/installer_{}.sh", self.interpreter, self.name);
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }

    /// Names of fields that came from an `[installers.<name>]` override.
    pub fn overridden_fields(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .sources
            .iter()
            .filter(|(_, s)| **s == FieldSource::Override)
            .map(|(k, _)| k.clone())
            .collect();
        for (present, name) in [
            (self.skip_reason.is_some(), "skip"),
            (self.expect_binary.is_some(), "expect_binary"),
            (self.verify_cmd.is_some(), "verify_cmd"),
            (self.run_as_root.is_some(), "run_as_root"),
            (self.memory_limit.is_some(), "memory_limit"),
            (self.network.is_some(), "network"),
        ] {
            if present {
                v.push(name.to_string());
            }
        }
        v.sort();
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn entry(url: &str) -> InstallerEntry {
        InstallerEntry {
            url: Some(url.to_string()),
            sha256: Some("abc".to_string()),
            version: None,
            enabled: true,
            tags: vec![],
            extra: HashMap::new(),
        }
    }

    const G: GlobalDefaults = GlobalDefaults { timeout_seconds: 300, retries: 3 };

    #[test]
    fn profile_applies_when_no_override() {
        let s = resolve_spec("ohmyzsh", &entry("https://x"), None, G);
        assert_eq!(s.interpreter, "sh");
        assert_eq!(s.args, vec!["--unattended", "--keep-zshrc"]);
        assert_eq!(s.sources["interpreter"], FieldSource::Profile);
        assert_eq!(s.timeout_seconds, 300);
        assert_eq!(s.retries, 3);
        assert!(s.skip_reason.is_none());
        assert_eq!(s.command_line(), "sh /tmp/installer_ohmyzsh.sh --unattended --keep-zshrc");
    }

    #[test]
    fn override_beats_profile_and_global() {
        let ovr = InstallerOverride {
            interpreter: Some("bash".into()),
            args: Some(vec!["--quiet".into()]),
            timeout_seconds: Some(42),
            retry: Some(0),
            env: [("ATUIN_NO_MODIFY_PATH".to_string(), "0".to_string())].into_iter().collect(),
            expect_binary: Some("atuin".into()),
            ..Default::default()
        };
        let s = resolve_spec("atuin", &entry("https://x"), Some(&ovr), G);
        assert_eq!(s.interpreter, "bash");
        assert_eq!(s.args, vec!["--quiet"]);
        assert_eq!(s.timeout_seconds, 42);
        assert_eq!(s.retries, 0);
        assert_eq!(s.env["ATUIN_NO_MODIFY_PATH"], "0");
        assert_eq!(s.sources["env"], FieldSource::Override);
        assert_eq!(s.expect_binary.as_deref(), Some("atuin"));
        let fields = s.overridden_fields();
        for f in ["interpreter", "args", "timeout_seconds", "retries", "env", "expect_binary"] {
            assert!(fields.iter().any(|x| x == f), "{f} missing from {fields:?}");
        }
        let t = s.to_test();
        assert_eq!(t.retry_count, 1);
        assert_eq!(t.interpreter, "bash");
        assert_eq!(t.expect_binary.as_deref(), Some("atuin"));
    }

    #[test]
    fn profile_timeout_floor_raises_global_but_never_lowers_it() {
        let s = resolve_spec("mdwb", &entry("https://x"), None, G);
        assert_eq!(s.timeout_seconds, 900);
        assert_eq!(s.sources["timeout_seconds"], FieldSource::Profile);
        let big = GlobalDefaults { timeout_seconds: 1200, retries: 3 };
        let s = resolve_spec("mdwb", &entry("https://x"), None, big);
        assert_eq!(s.timeout_seconds, 1200);
        assert_eq!(s.sources["timeout_seconds"], FieldSource::Global);
    }

    #[test]
    fn skip_override_carries_reason() {
        let ovr = InstallerOverride {
            skip: Some(true),
            skip_reason: Some("upstream gone".into()),
            ..Default::default()
        };
        let s = resolve_spec("x", &entry("https://x"), Some(&ovr), G);
        assert_eq!(s.skip_reason.as_deref(), Some("upstream gone"));
        let ovr = InstallerOverride { skip: Some(true), ..Default::default() };
        let s = resolve_spec("x", &entry("https://x"), Some(&ovr), G);
        assert!(s.skip_reason.unwrap().contains("override"));
    }
}
