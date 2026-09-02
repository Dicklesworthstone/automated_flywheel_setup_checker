//! Settings resolution matrix (C5T): defaults < file < AFSC_* env < CLI for every known key.

use automated_flywheel_setup_checker::config::{
    env_name_for, resolve, unknown_keys, CliOverrides, Config, Settings, Source,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use tempfile::NamedTempFile;

fn file_with(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::with_suffix(".toml").unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f
}

/// Flatten a TOML value into dotted keys.
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

fn lookup(value: &toml::Value, key: &str) -> toml::Value {
    let mut cur = value;
    for part in key.split('.') {
        cur = &cur[part];
    }
    cur.clone()
}

/// A distinct-from-default value of the same TOML type for each key.
fn alternative(key: &str, default: &toml::Value) -> Option<toml::Value> {
    Some(match default {
        toml::Value::Boolean(b) => toml::Value::Boolean(!b),
        toml::Value::Integer(i) => toml::Value::Integer(i + 7),
        toml::Value::Float(f) => toml::Value::Float(f + 0.5),
        toml::Value::String(_) => match key {
            "docker.pull_policy" => toml::Value::String("always".into()),
            "docker.network" => toml::Value::String("none".into()),
            "execution.order" => toml::Value::String("name".into()),
            "remediation.mode" => toml::Value::String("advisory".into()),
            "notifications.mode" => toml::Value::String("every_run".into()),
            "general.log_level" => toml::Value::String("debug".into()),
            _ => toml::Value::String(format!("alt-{}", key.replace('.', "-"))),
        },
        _ => return None,
    })
}

fn all_keys() -> Vec<(String, toml::Value)> {
    let table = toml::Value::try_from(Config::default()).unwrap();
    let mut flat = Vec::new();
    flatten(&table, "", &mut flat);
    flat
}

#[test]
fn every_key_can_be_overridden_by_env_and_is_marked_env() {
    for (key, default) in all_keys() {
        let Some(alt) = alternative(&key, &default) else { continue };
        let mut env = BTreeMap::new();
        let raw = match &alt {
            toml::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        env.insert(env_name_for(&key), raw);
        let s = resolve(None, &env, &CliOverrides::default())
            .unwrap_or_else(|e| panic!("resolve with env override for {key}: {e}"));
        let resolved = toml::Value::try_from(&s.config).unwrap();
        assert_eq!(lookup(&resolved, &key), alt, "env override for {key}");
        assert_eq!(s.source_of(&key), Source::Env, "source for {key}");
    }
}

#[test]
fn every_key_can_be_overridden_by_file_and_is_marked_file() {
    for (key, default) in all_keys() {
        let Some(alt) = alternative(&key, &default) else { continue };
        let (section, field) = key.rsplit_once('.').unwrap();
        let rendered = match &alt {
            toml::Value::String(s) => format!("{:?}", s),
            other => other.to_string(),
        };
        let f = file_with(&format!("[{section}]\n{field} = {rendered}\n"));
        let s = resolve(Some(f.path()), &BTreeMap::new(), &CliOverrides::default())
            .unwrap_or_else(|e| panic!("resolve with file override for {key}: {e}"));
        let resolved = toml::Value::try_from(&s.config).unwrap();
        assert_eq!(lookup(&resolved, &key), alt, "file override for {key}");
        assert_eq!(s.source_of(&key), Source::File, "source for {key}");
        assert!(s.unknown_keys.is_empty(), "{key} should be a known key");
    }
}

#[test]
fn cli_beats_env_beats_file_for_flagged_keys() {
    let f = file_with(
        "[execution]\nparallel = 2\nfail_fast = false\n\n[docker]\nimage = \"file:img\"\ntimeout_seconds = 100\n\n[general]\nacfs_repo = \"/from/file\"\ndata_dir = \"/file/data\"\nallow_file_urls = false\n",
    );
    let mut env = BTreeMap::new();
    env.insert("AFSC_EXECUTION_PARALLEL".into(), "3".into());
    env.insert("AFSC_DOCKER_IMAGE".into(), "env:img".into());
    env.insert("AFSC_DOCKER_TIMEOUT_SECONDS".into(), "200".into());
    env.insert("AFSC_GENERAL_ACFS_REPO".into(), "/from/env".into());
    let cli = CliOverrides {
        parallel: Some("4".into()),
        timeout_seconds: Some(300),
        fail_fast: Some(true),
        image: Some("cli:img".into()),
        data_dir: Some(PathBuf::from("/cli/data")),
        acfs_repo: Some(PathBuf::from("/from/cli")),
        allow_file_urls: Some(true),
        ..Default::default()
    };
    let s = resolve(Some(f.path()), &env, &cli).unwrap();
    assert_eq!(s.config.execution.parallel, 4);
    assert_eq!(s.config.docker.timeout_seconds, 300);
    assert!(s.config.execution.fail_fast);
    assert_eq!(s.config.docker.image, "cli:img");
    assert_eq!(s.config.general.data_dir, "/cli/data");
    assert_eq!(s.config.general.acfs_repo, PathBuf::from("/from/cli"));
    assert!(s.config.general.allow_file_urls);
    for key in [
        "execution.parallel",
        "docker.timeout_seconds",
        "execution.fail_fast",
        "docker.image",
        "general.data_dir",
        "general.acfs_repo",
        "general.allow_file_urls",
    ] {
        assert_eq!(s.source_of(key), Source::Cli, "{key}");
    }

    // Without CLI, env wins over file.
    let s = resolve(Some(f.path()), &env, &CliOverrides::default()).unwrap();
    assert_eq!(s.config.execution.parallel, 3);
    assert_eq!(s.config.docker.image, "env:img");
    assert_eq!(s.source_of("docker.image"), Source::Env);
    assert_eq!(s.source_of("general.data_dir"), Source::File);
    assert_eq!(s.config.general.data_dir, "/file/data");
}

#[test]
fn partial_sections_for_every_section_parse() {
    for section in ["general", "docker", "execution", "remediation", "notifications", "monitoring", "watchdog"] {
        let f = file_with(&format!("[{section}]\n"));
        let s = resolve(Some(f.path()), &BTreeMap::new(), &CliOverrides::default())
            .unwrap_or_else(|e| panic!("empty [{section}] should parse: {e}"));
        assert_eq!(
            toml::Value::try_from(&s.config).unwrap(),
            toml::Value::try_from(Config::default()).unwrap(),
            "empty [{section}] must equal defaults"
        );
    }
}

#[test]
fn unknown_keys_detected_at_every_level() {
    let raw: toml::Table = toml::from_str(
        "top = 1\n[general]\nnope = 2\n[docker]\nimage = \"x\"\n[installers.a]\ntimeout_seconds = 1\nbad = 2\n[installers.b.env]\nX = \"1\"\n",
    )
    .unwrap();
    assert_eq!(
        unknown_keys(&raw),
        vec!["general.nope".to_string(), "installers.a.bad".to_string(), "top".to_string()]
    );
}

#[test]
fn auto_parallelism_resolves_to_at_least_one() {
    let f = file_with("[execution]\nparallel = \"auto\"\n");
    let s = resolve(Some(f.path()), &BTreeMap::new(), &CliOverrides::default()).unwrap();
    assert!(s.config.execution.parallel >= 1);
    assert!(s.config.execution.parallel.resolve() <= 4);
    let bad = file_with("[execution]\nparallel = \"lots\"\n");
    let s = resolve(Some(bad.path()), &BTreeMap::new(), &CliOverrides::default()).unwrap();
    assert_eq!(s.config.execution.parallel, 1, "unparseable strings fall back to 1");
}

#[test]
fn defaults_helper_matches_resolve_none() {
    let a = Settings::defaults();
    let b = resolve(None, &BTreeMap::new(), &CliOverrides::default()).unwrap();
    assert_eq!(toml::Value::try_from(&a.config).unwrap(), toml::Value::try_from(&b.config).unwrap());
    assert!(a.sources.is_empty());
}
