//! Script provenance ledger: every verified installer script stored content-addressed at
//! `<data_dir>/scripts/<installer>/<sha256>.sh` with an `index.json` per installer, so drift can be
//! shown as a diff against what was last known-good instead of two opaque hashes.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Versions kept per installer (newest by `last_seen`).
pub const KEEP_PER_INSTALLER: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LedgerEntry {
    pub sha256: String,
    pub size: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Run id of the last run in which the installer passed with this script (if any)
    pub last_verified_pass_run: Option<String>,
    /// Where the bytes came from
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LedgerIndex {
    pub installer: String,
    pub entries: BTreeMap<String, LedgerEntry>,
}

/// Content-addressed store under `<data_dir>/scripts`.
#[derive(Debug, Clone)]
pub struct Ledger {
    root: PathBuf,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

impl Ledger {
    pub fn new(data_dir: &Path) -> Self {
        Self { root: data_dir.join("scripts") }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir(&self, installer: &str) -> PathBuf {
        self.root.join(installer)
    }

    fn index_path(&self, installer: &str) -> PathBuf {
        self.dir(installer).join("index.json")
    }

    pub fn script_path(&self, installer: &str, sha256: &str) -> PathBuf {
        self.dir(installer).join(format!("{sha256}.sh"))
    }

    pub fn load_index(&self, installer: &str) -> LedgerIndex {
        std::fs::read_to_string(self.index_path(installer))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| LedgerIndex {
                installer: installer.to_string(),
                entries: BTreeMap::new(),
            })
    }

    fn save_index(&self, index: &LedgerIndex) -> Result<()> {
        let path = self.index_path(&index.installer);
        let json = serde_json::to_string_pretty(index)?;
        crate::lock::write_atomic(&path, json.as_bytes())
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Record script bytes. `verified_pass_run` marks the script as known-good for that run.
    /// Returns the sha256. Keeps the newest `KEEP_PER_INSTALLER` versions.
    pub fn record(
        &self,
        installer: &str,
        bytes: &[u8],
        url: Option<&str>,
        verified_pass_run: Option<&str>,
    ) -> Result<String> {
        let sha = sha256_hex(bytes);
        let dir = self.dir(installer);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = self.script_path(installer, &sha);
        if !path.exists() {
            crate::lock::write_atomic(&path, bytes)?;
        }
        let now = Utc::now();
        let mut index = self.load_index(installer);
        let entry = index.entries.entry(sha.clone()).or_insert_with(|| LedgerEntry {
            sha256: sha.clone(),
            size: bytes.len() as u64,
            first_seen: now,
            last_seen: now,
            last_verified_pass_run: None,
            url: url.map(str::to_string),
        });
        entry.last_seen = now;
        entry.size = bytes.len() as u64;
        if let Some(run) = verified_pass_run {
            entry.last_verified_pass_run = Some(run.to_string());
        }
        if url.is_some() {
            entry.url = url.map(str::to_string);
        }
        self.prune_index(&mut index)?;
        self.save_index(&index)?;
        Ok(sha)
    }

    /// Mark an already-stored script as verified by a passing run.
    pub fn mark_verified(&self, installer: &str, sha256: &str, run_id: &str) -> Result<bool> {
        let mut index = self.load_index(installer);
        let Some(entry) = index.entries.get_mut(sha256) else { return Ok(false) };
        entry.last_verified_pass_run = Some(run_id.to_string());
        entry.last_seen = Utc::now();
        self.save_index(&index)?;
        Ok(true)
    }

    fn prune_index(&self, index: &mut LedgerIndex) -> Result<()> {
        if index.entries.len() <= KEEP_PER_INSTALLER {
            return Ok(());
        }
        let mut by_age: Vec<(String, DateTime<Utc>, bool)> = index
            .entries
            .values()
            .map(|e| (e.sha256.clone(), e.last_seen, e.last_verified_pass_run.is_some()))
            .collect();
        // Oldest first; verified scripts are pruned last.
        by_age.sort_by(|a, b| a.2.cmp(&b.2).then(a.1.cmp(&b.1)));
        let excess = index.entries.len() - KEEP_PER_INSTALLER;
        for (sha, _, _) in by_age.into_iter().take(excess) {
            index.entries.remove(&sha);
            let path = self.script_path(&index.installer, &sha);
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }

    /// The most recently verified-pass script for an installer.
    pub fn latest_verified(&self, installer: &str) -> Option<(LedgerEntry, Vec<u8>)> {
        let index = self.load_index(installer);
        let entry = index
            .entries
            .values()
            .filter(|e| e.last_verified_pass_run.is_some())
            .max_by_key(|e| e.last_seen)?
            .clone();
        let bytes = std::fs::read(self.script_path(installer, &entry.sha256)).ok()?;
        Some((entry, bytes))
    }

    /// The most recently seen script (verified or not).
    pub fn latest(&self, installer: &str) -> Option<(LedgerEntry, Vec<u8>)> {
        let index = self.load_index(installer);
        let entry = index.entries.values().max_by_key(|e| e.last_seen)?.clone();
        let bytes = std::fs::read(self.script_path(installer, &entry.sha256)).ok()?;
        Some((entry, bytes))
    }

    /// Bytes for a specific stored hash.
    pub fn get(&self, installer: &str, sha256: &str) -> Option<Vec<u8>> {
        std::fs::read(self.script_path(installer, sha256)).ok()
    }

    pub fn installers(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.root)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_marks_verified_and_prunes_oldest_unverified_first() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(dir.path());
        let sha_a =
            ledger.record("tool", b"#!/bin/bash\necho a\n", Some("https://x/a.sh"), None).unwrap();
        assert_eq!(sha_a, sha256_hex(b"#!/bin/bash\necho a\n"));
        assert!(ledger.script_path("tool", &sha_a).exists());
        assert!(ledger.latest_verified("tool").is_none());
        assert!(ledger.mark_verified("tool", &sha_a, "run-1").unwrap());
        assert!(!ledger.mark_verified("tool", "nope", "run-1").unwrap());
        let (entry, bytes) = ledger.latest_verified("tool").unwrap();
        assert_eq!(entry.last_verified_pass_run.as_deref(), Some("run-1"));
        assert_eq!(bytes, b"#!/bin/bash\necho a\n");

        // Six more unverified versions: the oldest unverified ones go, the verified one stays.
        for i in 0..6 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            ledger.record("tool", format!("echo v{i}\n").as_bytes(), None, None).unwrap();
        }
        let index = ledger.load_index("tool");
        assert_eq!(index.entries.len(), KEEP_PER_INSTALLER);
        assert!(index.entries.contains_key(&sha_a), "verified script survives pruning");
        assert_eq!(ledger.installers(), vec!["tool".to_string()]);
        let (latest, _) = ledger.latest("tool").unwrap();
        assert_ne!(latest.sha256, sha_a);
        // Re-recording the same bytes updates last_seen but not first_seen.
        let again = ledger.record("tool", b"#!/bin/bash\necho a\n", None, Some("run-2")).unwrap();
        assert_eq!(again, sha_a);
        assert_eq!(
            ledger.load_index("tool").entries[&sha_a].last_verified_pass_run.as_deref(),
            Some("run-2")
        );
    }
}
