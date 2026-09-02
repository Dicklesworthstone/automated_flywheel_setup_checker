//! Shared support for CLI integration tests.
//!
//! A [`Fixture`] owns a temp directory holding a synthetic ACFS repo (`acfs/checksums.yaml` in the
//! current format), a private HOME (so results and metrics never touch the real
//! `~/.local/share/afsc`), a config file, and mock installer scripts served via `file://` URLs.

#![allow(dead_code)]

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Output;
use tempfile::TempDir;

pub struct Fixture {
    pub root: TempDir,
    pub acfs: PathBuf,
    pub home: PathBuf,
    pub config: PathBuf,
    entries: Vec<(String, String, String)>,
    execution: Option<(usize, u32, bool)>,
}

impl Fixture {
    /// Create a fixture with an empty installer set and default execution settings
    /// (`parallel = 1`, `retry_transient = 1`, `fail_fast = false`).
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let acfs = root.path().join("acfs");
        let home = root.path().join("home");
        std::fs::create_dir_all(&acfs).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let config = root.path().join("config.toml");
        let mut f = Self { root, acfs, home, config, entries: Vec::new(), execution: None };
        f.set_execution(1, 1, false);
        f.write_files();
        f
    }

    /// Set `[execution]` values (parallel, retry_transient, fail_fast) and rewrite files.
    pub fn set_execution(&mut self, parallel: usize, retries: u32, fail_fast: bool) {
        self.execution = Some((parallel, retries, fail_fast));
        self.write_files();
    }

    /// Path to the fixture scripts directory.
    pub fn scripts_dir(&self) -> PathBuf {
        let d = self.root.path().join("scripts");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Add a mock installer with the given bash body; returns its sha256.
    pub fn add_installer(&mut self, name: &str, body: &str) -> String {
        let path = self.scripts_dir().join(format!("{name}.sh"));
        std::fs::write(&path, body).unwrap();
        let sha = sha256_file(&path);
        self.add_entry(name, &format!("file://{}", path.display()), &sha);
        sha
    }

    /// Add a mock installer whose pinned sha256 is deliberately wrong.
    pub fn add_installer_with_wrong_hash(&mut self, name: &str, body: &str) {
        let path = self.scripts_dir().join(format!("{name}.sh"));
        std::fs::write(&path, body).unwrap();
        self.add_entry(name, &format!("file://{}", path.display()), &"0".repeat(64));
    }

    /// Add an entry pointing at an arbitrary URL (e.g. an unreachable one).
    pub fn add_entry(&mut self, name: &str, url: &str, sha: &str) {
        self.entries.retain(|(n, _, _)| n != name);
        self.entries.push((name.to_string(), url.to_string(), sha.to_string()));
        self.write_files();
    }

    /// A passing installer that echoes its user, HOME, and args.
    pub fn add_pass(&mut self, name: &str) -> String {
        self.add_installer(
            name,
            "#!/bin/bash\necho \"mock installer $0 ran as $(id -un) HOME=$HOME args=$*\"\nexit 0\n",
        )
    }

    /// An installer that fails like apt with a missing package (category `dependency`).
    pub fn add_dependency_failure(&mut self, name: &str) -> String {
        self.add_installer(
            name,
            "#!/bin/bash\necho 'E: Unable to locate package foo' >&2\nexit 100\n",
        )
    }

    /// An installer that refuses to run as root on stdout only (category `permission`).
    pub fn add_root_refusal(&mut self, name: &str) -> String {
        self.add_installer(
            name,
            "#!/bin/bash\necho \"Don't run this script as root. Run as a regular user with sudo.\"\nexit 1\n",
        )
    }

    /// An installer that sleeps for `seconds` then exits 0.
    pub fn add_sleeper(&mut self, name: &str, seconds: u64) -> String {
        self.add_installer(name, &format!("#!/bin/bash\nsleep {seconds}\nexit 0\n"))
    }

    /// An installer that fails `failures` times with a transient network error, then passes.
    /// State lives in a counter file inside the fixture root.
    pub fn add_flaky(&mut self, name: &str, failures: u32) -> String {
        let counter = self.root.path().join(format!("{name}.counter"));
        let body = format!(
            "#!/bin/bash\nC=0\n[ -f '{c}' ] && C=$(cat '{c}')\nC=$((C+1))\necho $C > '{c}'\nif [ $C -le {n} ]; then echo 'curl: (7) Failed to connect: Connection refused' >&2; exit 7; fi\necho 'recovered on attempt' $C\nexit 0\n",
            c = counter.display(),
            n = failures
        );
        self.add_installer(name, &body)
    }

    /// An unreachable https URL (connection refused) with a dummy hash.
    pub fn add_unreachable(&mut self, name: &str) {
        self.add_entry(name, "https://127.0.0.1:9/nope.sh", &"1".repeat(64));
    }

    fn write_files(&self) {
        let mut yaml = String::from("# synthetic checksums.yaml (current ACFS format)\ninstallers:\n");
        for (name, url, sha) in &self.entries {
            yaml.push_str(&format!("  {name}:\n    url: \"{url}\"\n    sha256: \"{sha}\"\n\n"));
        }
        std::fs::write(self.acfs.join("checksums.yaml"), yaml).unwrap();

        let (parallel, retries, fail_fast) = self.execution.unwrap_or((1, 1, false));
        let config = format!(
            "[general]\nacfs_repo = \"{}\"\nlog_level = \"info\"\n\n[execution]\nparallel = {parallel}\nretry_transient = {retries}\nfail_fast = {fail_fast}\n",
            self.acfs.display()
        );
        std::fs::write(&self.config, config).unwrap();
    }

    /// Run the binary with the fixture's HOME and config; prints diagnostics on non-zero exit
    /// only when `expect_success` is true.
    pub fn run(&self, args: &[&str]) -> Output {
        let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("automated_flywheel_setup_checker");
        cmd.env("HOME", &self.home)
            .env("AFSC_ALLOW_LOCAL", "1")
            .env_remove("RUST_LOG")
            .arg("--config")
            .arg(&self.config)
            .args(args);
        let output = cmd.output().expect("failed to execute binary");
        eprintln!(
            "--- cli: {} ---\nexit: {:?}\nstdout:\n{}\nstderr:\n{}\n--- end ---",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

pub fn sha256_file(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    hex::encode(Sha256::digest(bytes))
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Every non-empty stdout line must parse as JSON; returns the parsed values in order.
pub fn jsonl_lines(output: &Output) -> Vec<serde_json::Value> {
    stdout(output)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("stdout line {i} is not JSON ({e}): {line:?}"))
        })
        .collect()
}

/// stdout must be exactly one JSON document.
pub fn json_doc(output: &Output) -> serde_json::Value {
    let text = stdout(output);
    let mut stream = serde_json::Deserializer::from_str(&text).into_iter::<serde_json::Value>();
    let first = stream
        .next()
        .expect("stdout should contain a JSON document")
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {text:?}"));
    assert!(stream.next().is_none(), "stdout contained more than one JSON document:\n{text}");
    first
}

/// Assert no ANSI escape sequences and no tracing timestamps leaked into stdout.
pub fn assert_no_log_noise(output: &Output) {
    let text = stdout(output);
    assert!(!text.contains('\u{1b}'), "ANSI escape in stdout:\n{text}");
    for line in text.lines() {
        assert!(
            !(line.contains(" WARN ") || line.contains(" INFO ") || line.contains(" DEBUG ")),
            "log line leaked into stdout: {line}"
        );
    }
}

pub fn results_of_kind<'a>(lines: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
    lines.iter().filter(|v| v["kind"] == kind).collect()
}

pub fn find_result<'a>(lines: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    lines
        .iter()
        .find(|v| v["kind"] == "result" && v["installer_name"] == name)
        .unwrap_or_else(|| panic!("no result line for {name}"))
}
