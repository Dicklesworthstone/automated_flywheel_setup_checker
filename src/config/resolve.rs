//! Settings resolution: defaults < config file < `AFSC_*` environment < explicit CLI flags.
//!
//! The typed [`Config`] stays the single schema. [`Settings`] wraps a fully resolved `Config`
//! together with provenance for every key (`config show --resolved`) and the list of unknown keys
//! found in the file (`config validate`). Resolution is explicit and testable: no environment is
//! read during deserialization.

use super::schema::Config;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where a resolved value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Default,
    File,
    Env,
    Cli,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::File => "file",
            Source::Env => "env",
            Source::Cli => "cli",
        }
    }
}

/// Environment variable prefix for overrides (`AFSC_DOCKER_IMAGE=ubuntu:24.04`).
pub const ENV_PREFIX: &str = "AFSC_";

/// CLI flags that override settings. `None` means "not passed".
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub parallel: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub fail_fast: Option<bool>,
    pub image: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub acfs_repo: Option<PathBuf>,
    pub remediation_mode: Option<String>,
    pub allow_file_urls: Option<bool>,
    pub log_level: Option<String>,
}

impl CliOverrides {
    fn entries(&self) -> Vec<(String, toml::Value)> {
        let mut v = Vec::new();
        if let Some(p) = &self.parallel {
            v.push(("execution.parallel".into(), parse_scalar(p)));
        }
        if let Some(t) = self.timeout_seconds {
            v.push(("docker.timeout_seconds".into(), toml::Value::Integer(t as i64)));
        }
        if let Some(f) = self.fail_fast {
            v.push(("execution.fail_fast".into(), toml::Value::Boolean(f)));
        }
        if let Some(i) = &self.image {
            v.push(("docker.image".into(), toml::Value::String(i.clone())));
        }
        if let Some(d) = &self.data_dir {
            v.push(("general.data_dir".into(), toml::Value::String(d.to_string_lossy().into())));
        }
        if let Some(d) = &self.acfs_repo {
            v.push(("general.acfs_repo".into(), toml::Value::String(d.to_string_lossy().into())));
        }
        if let Some(m) = &self.remediation_mode {
            v.push(("remediation.mode".into(), toml::Value::String(m.clone())));
        }
        if let Some(a) = self.allow_file_urls {
            v.push(("general.allow_file_urls".into(), toml::Value::Boolean(a)));
        }
        if let Some(l) = &self.log_level {
            v.push(("general.log_level".into(), toml::Value::String(l.clone())));
        }
        v
    }
}

/// Fully resolved settings with provenance.
#[derive(Debug, Clone)]
pub struct Settings {
    pub config: Config,
    /// Source per dotted key (`docker.image`, `installers.mdwb.timeout_seconds`, ...)
    pub sources: BTreeMap<String, Source>,
    /// Keys present in the config file that the schema does not know
    pub unknown_keys: Vec<String>,
    pub config_path: Option<PathBuf>,
}

impl Settings {
    /// Resolve from defaults only (no file, no env, no CLI).
    pub fn defaults() -> Self {
        resolve(None, &BTreeMap::new(), &CliOverrides::default()).expect("defaults always resolve")
    }

    /// Source of a dotted key (`Default` when never overridden).
    pub fn source_of(&self, key: &str) -> Source {
        self.sources.get(key).copied().unwrap_or(Source::Default)
    }

    /// Render TOML with a trailing `# source` comment on every value line.
    pub fn render_annotated(&self) -> Result<String> {
        let table = toml::Value::try_from(&self.config)?;
        let mut out = String::new();
        render_table(&table, "", &self.sources, &mut out);
        Ok(out)
    }
}

fn render_table(value: &toml::Value, prefix: &str, sources: &BTreeMap<String, Source>, out: &mut String) {
    let Some(table) = value.as_table() else { return };
    // Scalars first, then nested tables (TOML requires scalars before sub-tables).
    for (k, v) in table {
        if !v.is_table() {
            let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            let src = sources.get(&key).copied().unwrap_or(Source::Default);
            let rendered = render_scalar(v);
            out.push_str(&format!("{k} = {rendered}  # {}\n", src.as_str()));
        }
    }
    for (k, v) in table {
        if v.is_table() {
            let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            out.push_str(&format!("\n[{key}]\n"));
            render_table(v, &key, sources, out);
        }
    }
}

fn render_scalar(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => format!("{:?}", s),
        toml::Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(render_scalar).collect();
            format!("[{}]", inner.join(", "))
        }
        other => other.to_string(),
    }
}

/// Parse a scalar from an environment/CLI string: integer, float, bool, else string.
pub fn parse_scalar(s: &str) -> toml::Value {
    let t = s.trim();
    if let Ok(i) = t.parse::<i64>() {
        return toml::Value::Integer(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        if t.contains('.') {
            return toml::Value::Float(f);
        }
    }
    match t.to_ascii_lowercase().as_str() {
        "true" => return toml::Value::Boolean(true),
        "false" => return toml::Value::Boolean(false),
        _ => {}
    }
    toml::Value::String(t.to_string())
}

/// Dotted key → `AFSC_SECTION_KEY`.
pub fn env_name_for(key: &str) -> String {
    format!("{ENV_PREFIX}{}", key.replace('.', "_").to_ascii_uppercase())
}

fn flatten(value: &toml::Value, prefix: &str, out: &mut Vec<(String, toml::Value)>) {
    match value {
        toml::Value::Table(t) => {
            for (k, v) in t {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(v, &key, out);
            }
        }
        other => out.push((prefix.to_string(), other.clone())),
    }
}

fn set_path(root: &mut toml::Table, key: &str, value: toml::Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let mut cur = root;
    for part in &parts[..parts.len() - 1] {
        let entry = cur
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        cur = entry.as_table_mut().expect("table");
    }
    cur.insert(parts[parts.len() - 1].to_string(), value);
}

/// Keys of the schema (dotted), derived from the default config; `installers.<name>.*` is a
/// dynamic prefix and is validated separately.
fn known_keys() -> Vec<String> {
    let table = toml::Value::try_from(Config::default()).expect("default config serializes");
    let mut flat = Vec::new();
    flatten(&table, "", &mut flat);
    flat.into_iter().map(|(k, _)| k).collect()
}

/// Keys the schema knows under `installers.<name>`.
const INSTALLER_OVERRIDE_KEYS: &[&str] = &[
    "timeout_seconds",
    "retry",
    "interpreter",
    "args",
    "env",
    "skip",
    "skip_reason",
    "expect_binary",
    "verify_cmd",
    "version_cmd",
    "run_as_root",
    "memory_limit",
    "network",
];

/// Find keys in a raw file table that the schema does not know.
pub fn unknown_keys(raw: &toml::Table) -> Vec<String> {
    let known = known_keys();
    let mut flat = Vec::new();
    flatten(&toml::Value::Table(raw.clone()), "", &mut flat);
    let mut unknown = Vec::new();
    for (key, _) in flat {
        if known.iter().any(|k| k == &key) {
            continue;
        }
        if let Some(rest) = key.strip_prefix("installers.") {
            // installers.<name>.<field>[.<env-key>]
            let mut it = rest.splitn(3, '.');
            let _name = it.next();
            let field = it.next().unwrap_or("");
            if INSTALLER_OVERRIDE_KEYS.contains(&field) {
                continue;
            }
        }
        unknown.push(key);
    }
    unknown.sort();
    unknown
}

/// Load the raw file table (empty when `path` is `None`).
fn load_raw(path: Option<&Path>) -> Result<toml::Table> {
    match path {
        Some(p) => {
            let content = std::fs::read_to_string(p)
                .with_context(|| format!("Failed to read config file: {}", p.display()))?;
            let table: toml::Table = toml::from_str(&content)
                .with_context(|| format!("Failed to parse config file: {}", p.display()))?;
            Ok(table)
        }
        None => Ok(toml::Table::new()),
    }
}

/// Resolve settings from an optional file, an environment map, and CLI overrides.
///
/// `env` is passed explicitly (see [`env_map`]) so tests never touch the process environment.
pub fn resolve(
    path: Option<&Path>,
    env: &BTreeMap<String, String>,
    cli: &CliOverrides,
) -> Result<Settings> {
    let raw = load_raw(path)?;
    let unknown = unknown_keys(&raw);

    // 1. defaults
    let mut merged: toml::Table = toml::Value::try_from(Config::default())
        .context("default config serializes")?
        .as_table()
        .cloned()
        .unwrap_or_default();
    let mut sources: BTreeMap<String, Source> = BTreeMap::new();

    // 2. file (only keys actually present)
    let mut file_flat = Vec::new();
    flatten(&toml::Value::Table(raw.clone()), "", &mut file_flat);
    for (key, value) in file_flat {
        set_path(&mut merged, &key, value);
        sources.insert(key, Source::File);
    }

    // 3. environment (known keys only)
    for key in known_keys() {
        let name = env_name_for(&key);
        if let Some(raw_value) = env.get(&name) {
            set_path(&mut merged, &key, parse_scalar(raw_value));
            sources.insert(key, Source::Env);
        }
    }

    // 4. CLI
    for (key, value) in cli.entries() {
        set_path(&mut merged, &key, value);
        sources.insert(key, Source::Cli);
    }

    let config: Config = toml::Value::Table(merged)
        .try_into()
        .with_context(|| match path {
            Some(p) => format!("Failed to parse config file: {}", p.display()),
            None => "Failed to resolve configuration".to_string(),
        })?;

    Ok(Settings { config, sources, unknown_keys: unknown, config_path: path.map(Path::to_path_buf) })
}

/// Snapshot of the process environment restricted to `AFSC_*` variables.
pub fn env_map() -> BTreeMap<String, String> {
    std::env::vars().filter(|(k, _)| k.starts_with(ENV_PREFIX)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file_with(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn defaults_resolve_with_default_sources() {
        let s = Settings::defaults();
        assert_eq!(s.source_of("docker.image"), Source::Default);
        assert_eq!(s.config.docker.image, "afsc-base:latest");
        assert!(s.unknown_keys.is_empty());
    }

    #[test]
    fn partial_section_parses_and_marks_file_source() {
        let f = file_with("[docker]\ntimeout_seconds = 600\n");
        let s = resolve(Some(f.path()), &BTreeMap::new(), &CliOverrides::default()).unwrap();
        assert_eq!(s.config.docker.timeout_seconds, 600);
        assert_eq!(s.source_of("docker.timeout_seconds"), Source::File);
        assert_eq!(s.source_of("docker.image"), Source::Default);
        assert_eq!(s.config.docker.image, "afsc-base:latest");
    }

    #[test]
    fn env_overrides_file_and_cli_overrides_env() {
        let f = file_with("[execution]\nparallel = 2\n\n[docker]\nimage = \"ubuntu:22.04\"\n");
        let mut env = BTreeMap::new();
        env.insert("AFSC_EXECUTION_PARALLEL".to_string(), "3".to_string());
        env.insert("AFSC_DOCKER_IMAGE".to_string(), "ubuntu:24.04".to_string());
        let cli = CliOverrides { parallel: Some("4".into()), ..Default::default() };
        let s = resolve(Some(f.path()), &env, &cli).unwrap();
        assert_eq!(s.config.execution.parallel.resolve_with_cores(8), 4);
        assert_eq!(s.source_of("execution.parallel"), Source::Cli);
        assert_eq!(s.config.docker.image, "ubuntu:24.04");
        assert_eq!(s.source_of("docker.image"), Source::Env);
    }

    #[test]
    fn unknown_keys_are_reported_but_installer_overrides_are_known() {
        let f = file_with(
            "[general]\nbogus = 1\n\n[installers.mdwb]\ntimeout_seconds = 900\nenv = { FOO = \"bar\" }\n\n[installers.x]\nwhat = 1\n",
        );
        let s = resolve(Some(f.path()), &BTreeMap::new(), &CliOverrides::default()).unwrap();
        assert_eq!(s.unknown_keys, vec!["general.bogus".to_string(), "installers.x.what".to_string()]);
        assert_eq!(s.config.installers["mdwb"].timeout_seconds, Some(900));
        assert_eq!(s.config.installers["mdwb"].env.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn env_scalar_parsing() {
        assert_eq!(parse_scalar("4"), toml::Value::Integer(4));
        assert_eq!(parse_scalar("true"), toml::Value::Boolean(true));
        assert_eq!(parse_scalar("1.5"), toml::Value::Float(1.5));
        assert_eq!(parse_scalar("auto"), toml::Value::String("auto".into()));
        assert_eq!(env_name_for("docker.timeout_seconds"), "AFSC_DOCKER_TIMEOUT_SECONDS");
    }

    #[test]
    fn annotated_render_is_valid_toml_with_sources() {
        let f = file_with("[docker]\ntimeout_seconds = 600\n");
        let s = resolve(Some(f.path()), &BTreeMap::new(), &CliOverrides::default()).unwrap();
        let text = s.render_annotated().unwrap();
        let parsed: toml::Table = toml::from_str(&text).expect("annotated output parses as TOML");
        assert_eq!(parsed["docker"]["timeout_seconds"].as_integer(), Some(600));
        assert!(text.contains("timeout_seconds = 600  # file"));
        assert!(text.contains("image = \"afsc-base:latest\"  # default"));
    }

    #[test]
    fn empty_file_and_dev_null_resolve_to_defaults() {
        let f = file_with("");
        let s = resolve(Some(f.path()), &BTreeMap::new(), &CliOverrides::default()).unwrap();
        assert_eq!(s.config.execution.parallel.resolve_with_cores(4), 1);
        let s = resolve(Some(Path::new("/dev/null")), &BTreeMap::new(), &CliOverrides::default()).unwrap();
        assert_eq!(s.config.docker.timeout_seconds, 300);
    }

    #[test]
    fn shipped_default_toml_equals_config_default() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/default.toml");
        let s = resolve(Some(&path), &BTreeMap::new(), &CliOverrides::default()).unwrap();
        assert!(s.unknown_keys.is_empty(), "unknown keys in config/default.toml: {:?}", s.unknown_keys);
        let shipped = toml::Value::try_from(&s.config).unwrap();
        let defaults = toml::Value::try_from(Config::default()).unwrap();
        assert_eq!(shipped, defaults, "config/default.toml drifted from Config::default()");
    }

    #[test]
    fn type_errors_are_reported_with_the_file_path() {
        let f = file_with("[docker]\ntimeout_seconds = \"soon\"\n");
        let err = resolve(Some(f.path()), &BTreeMap::new(), &CliOverrides::default()).unwrap_err();
        assert!(err.to_string().contains("Failed to parse config file"));
    }
}
