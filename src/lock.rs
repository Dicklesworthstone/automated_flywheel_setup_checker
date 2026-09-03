//! Cross-process run lock.
//!
//! Two `check` processes (a systemd timer plus a manual run, say) must not interleave: they would
//! race on the metrics snapshot and double-build images. The lock is a pid file created with
//! `O_EXCL` under `<data_dir>/locks/`; a stale file whose owner pid is gone is reclaimed
//! automatically, so a SIGKILLed run never wedges the next one. Dependency-free and adequate for
//! a single host; it is not an NFS-safe lock.

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Information about the process holding a lock.
#[derive(Debug, Clone)]
pub struct LockHolder {
    pub pid: u32,
    pub since: String,
}

/// Held lock; removed on drop.
pub struct RunLock {
    path: PathBuf,
}

impl RunLock {
    /// Try to acquire `<dir>/<name>.lock`. Returns `Ok(Err(holder))` when another live process
    /// holds it.
    pub fn try_acquire(dir: &Path, name: &str) -> Result<std::result::Result<RunLock, LockHolder>> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create lock directory {}", dir.display()))?;
        let path = dir.join(format!("{name}.lock"));
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let _ =
                        writeln!(f, "{}\n{}", std::process::id(), chrono::Utc::now().to_rfc3339());
                    return Ok(Ok(RunLock { path }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = read_holder(&path);
                    match holder {
                        Some(h) if pid_alive(h.pid) => return Ok(Err(h)),
                        _ => {
                            // Stale (dead owner or unreadable): reclaim and retry once.
                            let _ = std::fs::remove_file(&path);
                            continue;
                        }
                    }
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("Failed to create lock {}", path.display()))
                }
            }
        }
        // Lost the race twice; report whoever holds it now.
        Ok(Err(read_holder(&path).unwrap_or(LockHolder { pid: 0, since: String::from("unknown") })))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn read_holder(path: &Path) -> Option<LockHolder> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let pid = lines.next()?.trim().parse().ok()?;
    let since = lines.next().unwrap_or("unknown").trim().to_string();
    Some(LockHolder { pid, since })
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if cfg!(target_os = "linux") {
        Path::new(&format!("/proc/{pid}")).exists()
    } else {
        true
    }
}

/// Write `contents` to `path` atomically (temp file in the same directory, then rename).
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    std::fs::write(&tmp, contents)
        .with_context(|| format!("Failed to write temp file {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_within_a_process_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let first = match RunLock::try_acquire(dir.path(), "run").unwrap() {
            Ok(lock) => lock,
            Err(holder) => panic!("first acquire refused by {holder:?}"),
        };
        let second = RunLock::try_acquire(dir.path(), "run").unwrap();
        let holder = second.err().expect("second acquire is refused");
        assert_eq!(holder.pid, std::process::id());
        drop(first);
        assert!(RunLock::try_acquire(dir.path(), "run").unwrap().is_ok());
    }

    #[test]
    fn stale_lock_with_dead_owner_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("run.lock"),
            format!("{}\n2020-01-01T00:00:00Z\n", u32::MAX - 3),
        )
        .unwrap();
        let lock = RunLock::try_acquire(dir.path(), "run").unwrap();
        assert!(lock.is_ok(), "dead owner must be reclaimed");
        let garbage = dir.path().join("x.lock");
        std::fs::write(&garbage, "not a pid").unwrap();
        assert!(RunLock::try_acquire(dir.path(), "x").unwrap().is_ok());
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("metrics.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
