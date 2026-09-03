//! Support for Docker integration tests.
//!
//! Tests are hermetic: mock installers live in a temp directory that is bind-mounted into the
//! container at `/fixtures`, and installers are referenced as `file:///fixtures/<name>.sh`, so no
//! network is needed beyond the (cached) base image. Gate: `AFSC_DOCKER_TESTS=1`.

#![allow(dead_code)]

use automated_flywheel_setup_checker::runner::{
    ContainerConfig, ContainerManager, ExecutionBackend, InstallerTest, PullPolicy, RunnerConfig,
};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

pub const MOUNT: &str = "/fixtures";

/// Whether Docker tests are enabled for this process.
pub fn enabled() -> bool {
    std::env::var("AFSC_DOCKER_TESTS").map(|v| v == "1").unwrap_or(false)
}

/// Skip helper: returns true (and logs) when Docker tests are disabled.
#[macro_export]
macro_rules! skip_unless_docker {
    () => {
        if !$crate::support::enabled() {
            eprintln!("SKIP: set AFSC_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }
    };
}

pub struct DockerFixture {
    pub dir: TempDir,
}

impl Default for DockerFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl DockerFixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        // Containers run as afsc-user (uid 1000); make the mount world-readable.
        let _ = std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode_compat(0o755));
        Self { dir }
    }

    /// Write a mock installer and return (container URL, sha256).
    pub fn add_installer(&self, name: &str, body: &str) -> (String, String) {
        let path = self.dir.path().join(format!("{name}.sh"));
        std::fs::write(&path, body).unwrap();
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode_compat(0o644));
        let sha = hex::encode(Sha256::digest(std::fs::read(&path).unwrap()));
        (format!("file://{MOUNT}/{name}.sh"), sha)
    }

    pub fn host_dir(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// Container config for the default (afsc-base) image with the fixture dir mounted read-only.
    pub fn container_config(&self) -> ContainerConfig {
        ContainerConfig {
            image: ContainerManager::AFSC_BASE_IMAGE.to_string(),
            volumes: vec![(format!("{}:ro", self.host_dir().display()), MOUNT.to_string())],
            // A cold worker builds the prepared image from scratch (apt, rustup, nvm) and can
            // need more than the 900 s default under load; the gate raises this via env.
            build_timeout_seconds: std::env::var("AFSC_DOCKER_TESTS_BUILD_TIMEOUT_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(900),
            ..Default::default()
        }
    }

    /// Runner config using the Docker backend and this fixture's mount.
    pub fn runner_config(&self, timeout: Duration) -> RunnerConfig {
        RunnerConfig {
            default_timeout: timeout,
            backend: ExecutionBackend::Docker {
                container_config: self.container_config(),
                pull_policy: PullPolicy::IfNotPresent,
            },
            ..Default::default()
        }
    }

    pub fn test(&self, name: &str, url: &str, sha: &str, timeout: Duration) -> InstallerTest {
        InstallerTest::new(name, url).with_sha256(sha).with_timeout(timeout).with_retry_count(1)
    }
}

/// Small shim so tests read naturally on non-unix too.
trait PermExt {
    fn from_mode_compat(mode: u32) -> std::fs::Permissions;
}
impl PermExt for std::fs::Permissions {
    #[cfg(unix)]
    fn from_mode_compat(mode: u32) -> std::fs::Permissions {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(mode)
    }
    #[cfg(not(unix))]
    fn from_mode_compat(_mode: u32) -> std::fs::Permissions {
        std::fs::metadata(".").unwrap().permissions()
    }
}

/// `docker ps -a` names matching a filter, via the docker CLI (independent of Bollard).
pub fn docker_ps_names(filter: &str) -> Vec<String> {
    let out = std::process::Command::new("docker")
        .args(["ps", "-a", "--filter", filter, "--format", "{{.Names}}"])
        .output()
        .expect("docker CLI");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

pub fn docker_inspect(id: &str, template: &str) -> String {
    let out = std::process::Command::new("docker")
        .args(["inspect", "--format", template, id])
        .output()
        .expect("docker CLI");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn docker_rm_force(name: &str) {
    let _ = std::process::Command::new("docker").args(["rm", "-f", name]).output();
}

/// Unwrap with the error printed to stdout first: remote test runners stream stdout but can
/// drop panic messages, so the reason must be visible before the panic.
pub fn must<T>(result: anyhow::Result<T>, what: &str) -> T {
    match result {
        Ok(v) => v,
        Err(e) => {
            println!("DOCKER TEST ERROR [{what}]: {e:#}");
            panic!("{what} failed: {e:#}");
        }
    }
}

/// Assert that no container matches `filter`, polling briefly: the daemon can still list a
/// container as "Removal In Progress" for a moment after `remove_container` returns.
pub fn assert_no_containers(filter: &str, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let names = docker_ps_names(filter);
        if names.is_empty() {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("{what}: containers still present for {filter}: {names:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
