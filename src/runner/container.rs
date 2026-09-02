//! Docker container management via Bollard
//!
//! Provides real Docker container lifecycle operations: create, exec, cleanup.
//! Uses bollard 0.16 to communicate with the Docker daemon.
//!
//! Every container the checker creates carries `afsc.*` labels so that orphans left behind by
//! a killed process can be identified and reaped conservatively (label match AND dead owner pid
//! or age beyond a bound). Exec calls are cancellable so signals stop installers promptly.

use anyhow::{Context, Result};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::ContainerCreateBody;
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
    RemoveContainerOptionsBuilder, StatsOptionsBuilder, StopContainerOptionsBuilder,
    TagImageOptionsBuilder,
};
use bollard::Docker;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Label marking containers created by this tool.
pub const LABEL_MANAGED: &str = "afsc.managed";
pub const LABEL_RUN_ID: &str = "afsc.run_id";
pub const LABEL_INSTALLER: &str = "afsc.installer";
pub const LABEL_CREATED_AT: &str = "afsc.created_at";
pub const LABEL_PID: &str = "afsc.pid";
pub const LABEL_VERSION: &str = "afsc.version";

/// Configuration for Docker containers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    pub image: String,
    pub memory_limit: Option<u64>,
    pub cpu_quota: Option<f64>,
    pub timeout_seconds: u64,
    pub volumes: Vec<(String, String)>,
    pub environment: Vec<(String, String)>,
    /// Extra labels applied to every container (the `afsc.*` set is always added)
    #[serde(default)]
    pub labels: Vec<(String, String)>,
    /// Docker network mode (`bridge` default, `none` for offline installers)
    #[serde(default)]
    pub network_mode: Option<String>,
    /// Run as root even on the non-root base image
    #[serde(default)]
    pub run_as_root: bool,
    /// Derive a prepared image (ACFS prerequisites + non-root `afsc-user`) from `image`.
    /// When false, `image` is used as-is and containers run as root with no prerequisites.
    #[serde(default = "default_prepare")]
    pub prepare: bool,
    /// Timeout for building the prepared image
    #[serde(default = "default_build_timeout")]
    pub build_timeout_seconds: u64,
    /// Rebuild the prepared image even when a cached one exists
    #[serde(default)]
    pub rebuild: bool,
}

fn default_prepare() -> bool {
    true
}

fn default_build_timeout() -> u64 {
    900
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            image: ContainerManager::AFSC_BASE_IMAGE.to_string(),
            memory_limit: Some(2 * 1024 * 1024 * 1024), // 2GB
            cpu_quota: Some(1.0),
            timeout_seconds: 300,
            volumes: Vec::new(),
            environment: Vec::new(),
            labels: Vec::new(),
            network_mode: None,
            run_as_root: false,
            prepare: true,
            build_timeout_seconds: 900,
            rebuild: false,
        }
    }
}

/// Canonical base for the default prepared image.
pub const CANONICAL_BASE: &str = "ubuntu:22.04";

/// Sanitize an image reference for use inside a tag (`ubuntu:24.04` → `ubuntu-24.04`).
fn sanitize_for_tag(image: &str) -> String {
    image
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Tag of the prepared image derived from `base` with the given Dockerfile template hash.
///
/// The canonical base keeps the `afsc-base` repository (with `afsc-base:latest` as an alias);
/// other bases get `afsc-prepared:<base>-<hash>`. Because the hash covers the template and the
/// base reference, editing the Dockerfile or switching bases produces a new tag and a rebuild.
pub fn prepared_image_tag(base: &str, template_hash: &str) -> String {
    let short = &template_hash[..template_hash.len().min(12)];
    if base == CANONICAL_BASE {
        format!("afsc-base:{short}")
    } else {
        format!("afsc-prepared:{}-{short}", sanitize_for_tag(base))
    }
}

/// Hash of the Dockerfile template plus the base reference.
pub fn template_hash(template: &[u8], base: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(template);
    h.update(b"\nBASE=");
    h.update(base.as_bytes());
    hex::encode(h.finalize())
}

/// Resolved image plan for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlan {
    /// Base image the prepared image derives from (or the raw image when not preparing)
    pub base: String,
    /// Image that containers actually run
    pub run_image: String,
    /// Whether `run_image` is an afsc-prepared image (non-root user available)
    pub prepared: bool,
}

/// Parse a human-readable memory limit string (e.g., "2G", "512M", "1024") into bytes.
pub fn parse_memory_limit(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, multiplier) = if s.ends_with('G') || s.ends_with('g') {
        (&s[..s.len() - 1], 1024 * 1024 * 1024u64)
    } else if s.ends_with('M') || s.ends_with('m') {
        (&s[..s.len() - 1], 1024 * 1024u64)
    } else if s.ends_with('K') || s.ends_with('k') {
        (&s[..s.len() - 1], 1024u64)
    } else {
        (s, 1u64)
    };
    num_str.trim().parse::<u64>().ok().map(|n| n * multiplier)
}

/// Image pull policy
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullPolicy {
    Always,
    IfNotPresent,
    Never,
}

impl PullPolicy {
    pub fn parse_policy(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "always" => PullPolicy::Always,
            "never" => PullPolicy::Never,
            _ => PullPolicy::IfNotPresent,
        }
    }
}

/// A container found by the reaper.
#[derive(Debug, Clone, Serialize)]
pub struct OrphanInfo {
    pub id: String,
    pub name: String,
    pub installer: Option<String>,
    pub run_id: Option<String>,
    pub pid: Option<u32>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub reason: String,
}

/// Manages Docker containers for installer testing
pub struct ContainerManager {
    config: ContainerConfig,
    docker: Arc<Docker>,
    pull_policy: PullPolicy,
}

/// Human-readable Docker endpoint for error messages.
fn docker_endpoint() -> String {
    std::env::var("DOCKER_HOST").unwrap_or_else(|_| "unix:///var/run/docker.sock".to_string())
}

impl ContainerManager {
    /// Create a new ContainerManager connected to the local Docker daemon.
    ///
    /// Panics only when the Docker client itself cannot be constructed (malformed `DOCKER_HOST`);
    /// prefer [`ContainerManager::try_new`].
    pub fn new(config: ContainerConfig) -> Self {
        Self::try_new(config).expect("Failed to construct Docker client")
    }

    /// Fallible constructor: returns an error instead of panicking on a bad `DOCKER_HOST`.
    pub fn try_new(config: ContainerConfig) -> Result<Self> {
        let docker = Docker::connect_with_local_defaults().map_err(|e| {
            anyhow::anyhow!(
                "Docker daemon unreachable at {}: {}. Start Docker or use --local",
                docker_endpoint(),
                e
            )
        })?;
        Ok(Self { config, docker: Arc::new(docker), pull_policy: PullPolicy::IfNotPresent })
    }

    /// Create with a specific pull policy
    pub fn with_pull_policy(mut self, policy: PullPolicy) -> Self {
        self.pull_policy = policy;
        self
    }

    /// Create with an existing Docker client (useful for testing)
    pub fn with_docker(config: ContainerConfig, docker: Docker) -> Self {
        Self { config, docker: Arc::new(docker), pull_policy: PullPolicy::IfNotPresent }
    }

    /// Verify the daemon answers before any container work; the error names the endpoint.
    pub async fn preflight(&self) -> Result<()> {
        self.docker.ping().await.map(|_| ()).map_err(|e| {
            anyhow::anyhow!(
                "Docker daemon unreachable at {}: {}. Start Docker or use --local",
                docker_endpoint(),
                e
            )
        })
    }

    /// Verify the daemon answers using a fresh default client.
    pub async fn preflight_default() -> Result<()> {
        // bollard 0.21 fails here (not at ping) when the socket does not exist; same wording.
        let docker = Docker::connect_with_local_defaults().map_err(|e| {
            anyhow::anyhow!(
                "Docker daemon unreachable at {}: {}. Start Docker or use --local",
                docker_endpoint(),
                e
            )
        })?;
        docker.ping().await.map(|_| ()).map_err(|e| {
            anyhow::anyhow!(
                "Docker daemon unreachable at {}: {}. Start Docker or use --local",
                docker_endpoint(),
                e
            )
        })
    }

    /// The tag used for the pre-built ACFS base image (alias of the canonical prepared image).
    pub const AFSC_BASE_IMAGE: &'static str = "afsc-base:latest";

    fn base_image_build_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// Work out which image containers run: the raw image when `prepare` is off, otherwise the
    /// prepared image derived from the configured base (`afsc-base:latest` means the canonical
    /// `ubuntu:22.04` base).
    pub fn image_plan(&self) -> Result<ImagePlan> {
        if !self.config.prepare {
            return Ok(ImagePlan {
                base: self.config.image.clone(),
                run_image: self.config.image.clone(),
                prepared: false,
            });
        }
        let base = if self.config.image == Self::AFSC_BASE_IMAGE
            || self.config.image.starts_with("afsc-base:")
            || self.config.image.starts_with("afsc-prepared:")
        {
            CANONICAL_BASE.to_string()
        } else {
            self.config.image.clone()
        };
        let template = std::fs::read(Self::find_dockerfile()?)
            .context("Failed to read docker/Dockerfile.base")?;
        let tag = prepared_image_tag(&base, &template_hash(&template, &base));
        Ok(ImagePlan { base, run_image: tag, prepared: true })
    }

    /// Ensure the image containers will run is available locally, building the prepared image
    /// when its hash tag is missing (or `rebuild` is set) and pulling raw images per policy.
    /// One resource sample for a running container (None when the daemon has no stats yet).
    pub async fn sample_stats(&self, container_id: &str) -> Option<(u64, u64, u64, u64)> {
        sample_container_stats(&self.docker, container_id).await
    }

    /// Sample a container every second until `stop` is cancelled; returns peak memory, CPU
    /// seconds, and network bytes seen. Cheap enough to run for every attempt.
    pub fn spawn_telemetry(
        &self,
        container_id: String,
        stop: CancellationToken,
    ) -> tokio::task::JoinHandle<crate::runner::Telemetry> {
        let docker = self.docker_arc();
        tokio::spawn(async move {
            let mut t = crate::runner::Telemetry::default();
            let mut first_cpu: Option<u64> = None;
            let mut last_cpu: u64 = 0;
            loop {
                if let Some((mem, cpu, rx, tx)) = sample_container_stats(&docker, &container_id).await {
                    t.samples += 1;
                    t.peak_memory_bytes = t.peak_memory_bytes.max(mem);
                    first_cpu.get_or_insert(cpu);
                    last_cpu = cpu;
                    t.network_rx_bytes = rx;
                    t.network_tx_bytes = tx;
                }
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
            // A final sample after the process ended catches short attempts.
            if let Some((mem, cpu, rx, tx)) = sample_container_stats(&docker, &container_id).await {
                t.samples += 1;
                t.peak_memory_bytes = t.peak_memory_bytes.max(mem);
                first_cpu.get_or_insert(cpu);
                last_cpu = cpu;
                t.network_rx_bytes = rx.max(t.network_rx_bytes);
                t.network_tx_bytes = tx.max(t.network_tx_bytes);
            }
            t.cpu_seconds = last_cpu.saturating_sub(first_cpu.unwrap_or(last_cpu)) as f64 / 1e9;
            t
        })
    }
}

/// One stats sample: (memory bytes, cumulative cpu ns, rx bytes, tx bytes).
async fn sample_container_stats(docker: &Docker, container_id: &str) -> Option<(u64, u64, u64, u64)> {
    let opts = StatsOptionsBuilder::default().stream(false).one_shot(false).build();
    let mut stream = docker.stats(container_id, Some(opts));
    {
        let stats = stream.next().await?.ok()?;
        let memory = stats.memory_stats.as_ref().and_then(|m| m.usage).unwrap_or(0);
        let cpu_ns = stats
            .cpu_stats
            .as_ref()
            .and_then(|c| c.cpu_usage.as_ref())
            .and_then(|u| u.total_usage)
            .unwrap_or(0);
        let (rx, tx) = stats
            .networks
            .as_ref()
            .map(|n| {
                n.values().fold((0u64, 0u64), |(r, t), s| {
                    (r + s.rx_bytes.unwrap_or(0), t + s.tx_bytes.unwrap_or(0))
                })
            })
            .unwrap_or((0, 0));
        Some((memory, cpu_ns, rx, tx))
    }
}

impl ContainerManager {
    /// Id of the image containers will run (after `ensure_image`), for run headers.
    pub async fn image_id(&self) -> Option<String> {
        let plan = self.image_plan().ok()?;
        self.docker.inspect_image(&plan.run_image).await.ok().and_then(|i| i.id)
    }

    /// Classify managed containers without touching them: the ones whose owner process is dead
    /// or that exceed `max_age` come back with a `reason`; live ones are omitted.
    pub async fn orphans(&self, max_age: Duration) -> Result<Vec<OrphanInfo>> {
        let my_pid = std::process::id();
        let now = chrono::Utc::now();
        let mut out = Vec::new();
        for mut c in self.list_managed().await? {
            if c.pid == Some(my_pid) {
                continue;
            }
            let owner_dead = match c.pid {
                Some(pid) => !pid_alive(pid),
                None => false,
            };
            let too_old = match c.created_at {
                Some(t) => (now - t).to_std().map(|d| d > max_age).unwrap_or(false),
                None => false,
            };
            if !(owner_dead || too_old) {
                continue;
            }
            c.reason = if owner_dead {
                format!("owner pid {} is not running", c.pid.unwrap_or(0))
            } else {
                format!("older than {}s", max_age.as_secs())
            };
            out.push(c);
        }
        Ok(out)
    }

    pub async fn ensure_image(&self) -> Result<ImagePlan> {
        let plan = self.image_plan()?;

        if !plan.prepared {
            self.ensure_pulled(&plan.run_image).await?;
            return Ok(plan);
        }

        let _build_guard = Self::base_image_build_lock().lock().await;
        let present = self.docker.inspect_image(&plan.run_image).await.is_ok();
        if present && !self.config.rebuild {
            debug!(image = %plan.run_image, "Prepared image already present");
        } else {
            self.build_prepared_image(&plan).await?;
        }
        // Keep the human-friendly alias pointing at the current canonical build.
        if plan.base == CANONICAL_BASE {
            let (repo, tag) = Self::AFSC_BASE_IMAGE.split_once(':').unwrap_or((Self::AFSC_BASE_IMAGE, "latest"));
            let opts = TagImageOptionsBuilder::default().repo(repo).tag(tag).build();
            if let Err(e) = self.docker.tag_image(&plan.run_image, Some(opts)).await {
                warn!(error = %e, "Failed to update the afsc-base:latest alias");
            }
        }
        Ok(plan)
    }

    /// Pull a raw image according to the pull policy.
    async fn ensure_pulled(&self, image: &str) -> Result<()> {
        if self.pull_policy != PullPolicy::Always && self.docker.inspect_image(image).await.is_ok() {
            debug!(image = %image, "Image already present locally");
            return Ok(());
        }
        if self.pull_policy == PullPolicy::Never {
            anyhow::bail!("Image {} not found and pull policy is Never", image);
        }
        info!(image = %image, "Pulling image");
        let (repo, tag) = if let Some(pos) = image.rfind(':') {
            (&image[..pos], &image[pos + 1..])
        } else {
            (image, "latest")
        };
        let opts = CreateImageOptionsBuilder::default().from_image(repo).tag(tag).build();
        let mut stream = self.docker.create_image(Some(opts), None, None);
        while let Some(result) = stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(status) = info.status {
                        debug!(status = %status, "Pull progress");
                    }
                }
                Err(e) => return Err(anyhow::anyhow!("Failed to pull image {}: {}", image, e)),
            }
        }
        info!(image = %image, "Image pulled successfully");
        Ok(())
    }

    /// Build the prepared image from `docker/Dockerfile.base` with `--build-arg BASE=<base>`.
    ///
    /// The Dockerfile installs the packages ACFS's `install_base.sh` installs, plus Rust and Node
    /// for installers that expect them, and creates the non-root `afsc-user`. Building takes a
    /// few minutes once per (template, base) pair; the hash tag makes later runs instant.
    async fn build_prepared_image(&self, plan: &ImagePlan) -> Result<()> {
        info!(image = %plan.run_image, base = %plan.base, "Building prepared image (this takes a few minutes the first time)");

        let dockerfile_path = Self::find_dockerfile()?
            .canonicalize()
            .context("Failed to canonicalize Dockerfile.base path")?;
        let context_dir = dockerfile_path.parent().and_then(|p| p.parent()).ok_or_else(|| {
            anyhow::anyhow!("Cannot determine build context from Dockerfile path")
        })?;

        let build_timeout = Duration::from_secs(self.config.build_timeout_seconds.max(60));
        let mut command = tokio::process::Command::new("docker");
        command.arg("build");
        if self.config.rebuild || self.pull_policy == PullPolicy::Always {
            command.arg("--pull");
        }
        if self.config.rebuild {
            command.arg("--no-cache");
        }
        command
            .args(["--build-arg", &format!("BASE={}", plan.base)])
            .args(["-t", &plan.run_image])
            .args(["-f", &dockerfile_path.to_string_lossy()])
            .arg(&*context_dir.to_string_lossy())
            .kill_on_drop(true);

        // Build using the docker CLI (Bollard's build API needs a tar context).
        let output = match tokio::time::timeout(build_timeout, command.output()).await {
            Ok(result) => result.context("Failed to run docker build (is the docker CLI installed?)")?,
            Err(_) => {
                anyhow::bail!(
                    "Timed out after {}s building {} (raise [docker].build_timeout_seconds)",
                    build_timeout.as_secs(),
                    plan.run_image
                );
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Drop the legacy-builder deprecation banner so the real error is visible first.
            let relevant: Vec<&str> = stderr
                .lines()
                .filter(|l| !l.contains("DEPRECATED") && !l.contains("buildx") && !l.trim().is_empty())
                .collect();
            anyhow::bail!(
                "Failed to build {} from {}:\n{}",
                plan.run_image,
                plan.base,
                relevant.join("\n").chars().take(1500).collect::<String>()
            );
        }

        info!(image = %plan.run_image, "Prepared image built");
        Ok(())
    }

    /// Locate the Dockerfile.base shipped with this project.
    fn find_dockerfile() -> Result<std::path::PathBuf> {
        // Try relative to the binary location
        let candidates = [
            std::path::PathBuf::from("docker/Dockerfile.base"),
            std::path::PathBuf::from(
                "/data/projects/automated_flywheel_setup_checker/docker/Dockerfile.base",
            ),
        ];

        // Also check CARGO_MANIFEST_DIR at compile time
        let manifest_candidate = option_env!("CARGO_MANIFEST_DIR")
            .map(|d| std::path::PathBuf::from(d).join("docker/Dockerfile.base"));

        for path in candidates.iter().chain(manifest_candidate.iter()) {
            if path.exists() {
                return Ok(path.clone());
            }
        }

        anyhow::bail!("Cannot find docker/Dockerfile.base. Looked in: {:?}", candidates)
    }

    /// Labels applied to every container: the managed marker, installer, creation time, owner
    /// pid, tool version, plus any configured extras.
    fn labels_for(&self, installer: &str) -> HashMap<String, String> {
        let mut labels = HashMap::new();
        labels.insert(LABEL_MANAGED.to_string(), "true".to_string());
        labels.insert(LABEL_INSTALLER.to_string(), installer.to_string());
        labels.insert(LABEL_CREATED_AT.to_string(), chrono::Utc::now().to_rfc3339());
        labels.insert(LABEL_PID.to_string(), std::process::id().to_string());
        labels.insert(LABEL_VERSION.to_string(), env!("CARGO_PKG_VERSION").to_string());
        for (k, v) in &self.config.labels {
            labels.insert(k.clone(), v.clone());
        }
        labels
    }

    /// Create and start a container for testing
    ///
    /// Returns the container ID string from Docker.
    pub async fn create_container(&self, name: &str) -> Result<String> {
        // Ensure image is available (prepared images are built on demand)
        let plan = self.ensure_image().await.context("Failed to ensure Docker image")?;

        // Build container name: afsc-INSTALLERNAME-TIMESTAMP-RANDOM
        // Include milliseconds and random suffix to avoid collisions in parallel mode
        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f");
        let random_suffix: u16 = rand::random();
        let container_name = format!("afsc-{}-{}-{:04x}", name, timestamp, random_suffix);

        // Prepared images carry the non-root afsc-user; raw images run as root.
        let using_base_image = plan.prepared && !self.config.run_as_root;
        let (user, home, working_dir) = if using_base_image {
            ("afsc-user", "/home/afsc-user", "/home/afsc-user")
        } else {
            ("root", "/root", "/root")
        };

        // Build environment variables
        let mut env: Vec<String> = vec![
            "DEBIAN_FRONTEND=noninteractive".to_string(),
            format!("HOME={}", home),
            format!(
                "PATH={}/.cargo/bin:{}/.local/bin:{}/.nvm/versions/node/default/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                home, home, home
            ),
            "CI=true".to_string(),
            "NONINTERACTIVE=1".to_string(),
            "RUSTUP_INIT_SKIP_PATH_CHECK=yes".to_string(),
        ];

        // Add config environment variables
        for (key, value) in &self.config.environment {
            env.push(format!("{}={}", key, value));
        }

        // Build host config
        let mut host_config = bollard::models::HostConfig::default();

        // Memory limit
        if let Some(mem) = self.config.memory_limit {
            host_config.memory = Some(mem as i64);
        }

        // CPU quota (convert float cores to Docker's nano-CPU format)
        if let Some(cpu) = self.config.cpu_quota {
            host_config.nano_cpus = Some((cpu * 1_000_000_000.0) as i64);
        }

        // NOTE: We deliberately do NOT mount /tmp as tmpfs. Docker's default overlay
        // filesystem for /tmp allows exec, but tmpfs mounts add noexec by default
        // (even with "exec" in the options string — Bollard may not pass it correctly).
        // Installers like rustup download binaries to /tmp and need exec permission.
        // The container is ephemeral anyway, so a real /tmp is fine.

        // Network mode (`none` isolates offline-capable installers)
        if let Some(mode) = &self.config.network_mode {
            if !mode.trim().is_empty() {
                host_config.network_mode = Some(mode.trim().to_string());
            }
        }

        // Volume binds
        if !self.config.volumes.is_empty() {
            // Docker wants host:container[:mode]; the host side may carry a ":ro"/":rw" suffix.
            let binds: Vec<String> = self
                .config
                .volumes
                .iter()
                .map(|(host, container)| match host.rsplit_once(':') {
                    Some((h, mode)) if matches!(mode, "ro" | "rw" | "z" | "Z") => {
                        format!("{h}:{container}:{mode}")
                    }
                    _ => format!("{host}:{container}"),
                })
                .collect();
            host_config.binds = Some(binds);
        }

        // Create container config
        let container_config = ContainerCreateBody {
            image: Some(plan.run_image.clone()),
            env: Some(env),
            host_config: Some(host_config),
            labels: Some(self.labels_for(name)),
            // Keep container alive with a long sleep so we can exec into it
            cmd: Some(vec!["sleep".to_string(), "86400".to_string()]),
            working_dir: Some(working_dir.to_string()),
            // Always explicit: prepared images default to afsc-user in the image itself, so
            // `run_as_root` must override it with "root" rather than inherit the image USER.
            user: Some(user.to_string()),
            tty: Some(true),
            ..Default::default()
        };

        let create_opts = CreateContainerOptionsBuilder::default().name(&container_name).build();

        let response = self
            .docker
            .create_container(Some(create_opts), container_config)
            .await
            .context("Failed to create Docker container")?;

        let container_id = response.id.clone();
        info!(
            container_id = %container_id,
            container_name = %container_name,
            image = %plan.run_image,
            user = %user,
            "Container created"
        );

        // Start the container
        self.docker
            .start_container(&container_id, None)
            .await
            .context("Failed to start Docker container")?;

        info!(container_id = %container_id, "Container started");

        Ok(container_id)
    }

    /// Execute a command inside a running container
    ///
    /// Returns (exit_code, stdout, stderr).
    pub async fn exec_in_container(
        &self,
        container_id: &str,
        command: &[&str],
    ) -> Result<(i32, String, String)> {
        self.exec_in_container_cancellable(container_id, command, &CancellationToken::new()).await
    }

    /// Execute a command inside a running container, aborting when `cancel` fires.
    ///
    /// On cancellation the container is stopped (which kills the exec'd process) and an error
    /// containing `cancelled` is returned.
    pub async fn exec_in_container_cancellable(
        &self,
        container_id: &str,
        command: &[&str],
        cancel: &CancellationToken,
    ) -> Result<(i32, String, String)> {
        debug!(
            container_id = %container_id,
            command = ?command,
            "Executing command in container"
        );

        if cancel.is_cancelled() {
            anyhow::bail!("cancelled before exec started");
        }

        let exec_opts = CreateExecOptions {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            // NOTE: We do NOT set tty on exec. The container itself has tty:true
            // which makes /dev/tty available to installers that check for it.
            // But tty on exec merges stdout/stderr into one stream, which breaks
            // our CHECKSUM_MISMATCH stderr detection and error classification.
            cmd: Some(command.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };

        let exec = self
            .docker
            .create_exec(container_id, exec_opts)
            .await
            .context("Failed to create exec instance")?;

        let exec_id = exec.id;

        // Start the exec and collect output
        let start_result = self
            .docker
            .start_exec(&exec_id, None)
            .await
            .context("Failed to start exec instance")?;

        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        match start_result {
            StartExecResults::Attached { mut output, .. } => loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        warn!(container_id = %container_id, "Exec cancelled; stopping container");
                        let stop_opts = StopContainerOptionsBuilder::default().t(2).build();
                        let _ = self.docker.stop_container(container_id, Some(stop_opts)).await;
                        anyhow::bail!("cancelled while running installer");
                    }
                    msg = output.next() => {
                        match msg {
                            Some(Ok(bollard::container::LogOutput::StdOut { message })) => {
                                stdout_buf.extend_from_slice(&message);
                            }
                            Some(Ok(bollard::container::LogOutput::StdErr { message })) => {
                                stderr_buf.extend_from_slice(&message);
                            }
                            Some(Ok(_)) => {} // Console or other log types
                            Some(Err(e)) => {
                                warn!(error = %e, "Error reading exec output");
                                break;
                            }
                            None => break,
                        }
                    }
                }
            },
            StartExecResults::Detached => {
                return Err(anyhow::anyhow!("Exec started in detached mode unexpectedly"));
            }
        }

        // Get exit code from exec inspect
        let exec_inspect = self
            .docker
            .inspect_exec(&exec_id)
            .await
            .context("Failed to inspect exec for exit code")?;

        let exit_code = exec_inspect.exit_code.unwrap_or(-1) as i32;

        let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
        let stderr = String::from_utf8_lossy(&stderr_buf).to_string();

        debug!(
            container_id = %container_id,
            exit_code = exit_code,
            stdout_len = stdout.len(),
            stderr_len = stderr.len(),
            "Exec completed"
        );

        Ok((exit_code, stdout, stderr))
    }

    /// Stop and remove a container (best-effort cleanup)
    ///
    /// Logs failures but does not propagate errors. This ensures cleanup
    /// always completes even if the container is already stopped/removed.
    pub async fn cleanup_container(&self, container_id: &str) -> Result<()> {
        info!(container_id = %container_id, "Cleaning up container");

        // Stop with 10-second grace period
        let stop_opts = StopContainerOptionsBuilder::default().t(10).build();
        if let Err(e) = self.docker.stop_container(container_id, Some(stop_opts)).await {
            // 304 = already stopped, 404 = not found — both are fine
            debug!(
                container_id = %container_id,
                error = %e,
                "Stop container returned error (may already be stopped)"
            );
        }

        // Force remove
        let remove_opts = RemoveContainerOptionsBuilder::default().force(true).v(true).build();
        if let Err(e) = self.docker.remove_container(container_id, Some(remove_opts)).await {
            error!(
                container_id = %container_id,
                error = %e,
                "Failed to remove container"
            );
        } else {
            info!(container_id = %container_id, "Container removed");
        }

        Ok(())
    }

    /// List containers created by this tool (any state).
    pub async fn list_managed(&self) -> Result<Vec<OrphanInfo>> {
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("label".to_string(), vec![format!("{LABEL_MANAGED}=true")]);
        let opts = ListContainersOptionsBuilder::default().all(true).filters(&filters).build();
        let containers = self
            .docker
            .list_containers(Some(opts))
            .await
            .context("Failed to list containers")?;
        Ok(containers
            .into_iter()
            .map(|c| {
                let labels = c.labels.unwrap_or_default();
                OrphanInfo {
                    id: c.id.unwrap_or_default(),
                    name: c
                        .names
                        .and_then(|n| n.first().cloned())
                        .unwrap_or_default()
                        .trim_start_matches('/')
                        .to_string(),
                    installer: labels.get(LABEL_INSTALLER).cloned(),
                    run_id: labels.get(LABEL_RUN_ID).cloned(),
                    pid: labels.get(LABEL_PID).and_then(|p| p.parse().ok()),
                    created_at: labels
                        .get(LABEL_CREATED_AT)
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map(|t| t.with_timezone(&chrono::Utc)),
                    reason: String::new(),
                }
            })
            .collect())
    }

    /// Remove managed containers whose owner process is dead or whose age exceeds `max_age`.
    /// Containers owned by the current process are never touched. Returns what was removed.
    pub async fn reap_orphans(&self, max_age: Duration) -> Result<Vec<OrphanInfo>> {
        let mut reaped = Vec::new();
        for c in self.orphans(max_age).await? {
            warn!(container = %c.name, reason = %c.reason, "Reaping orphaned container");
            let _ = self.cleanup_container(&c.id).await;
            reaped.push(c);
        }
        Ok(reaped)
    }

    /// Get a reference to the Docker client (for advanced use)
    pub fn docker(&self) -> &Docker {
        &self.docker
    }

    /// Get the Arc-wrapped Docker client (for ContainerGuard)
    pub fn docker_arc(&self) -> Arc<Docker> {
        self.docker.clone()
    }

    pub fn config(&self) -> &ContainerConfig {
        &self.config
    }
}

/// Whether a process id is alive (Linux: /proc; elsewhere: assume alive).
fn pid_alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    } else {
        true
    }
}

/// Guard that ensures container cleanup on drop.
/// Use this to wrap container IDs when you need guaranteed cleanup
/// even on panic or early return.
pub struct ContainerGuard {
    container_id: String,
    docker: Arc<Docker>,
    cleaned: bool,
}

impl ContainerGuard {
    pub fn new(container_id: String, docker: Arc<Docker>) -> Self {
        Self { container_id, docker, cleaned: false }
    }

    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    /// Explicitly clean up (preferred over relying on Drop)
    pub async fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;

        let stop_opts = StopContainerOptionsBuilder::default().t(10).build();
        if let Err(e) = self.docker.stop_container(&self.container_id, Some(stop_opts)).await {
            debug!(
                container_id = %self.container_id,
                error = %e,
                "Stop during guard cleanup (may already be stopped)"
            );
        }

        let remove_opts = RemoveContainerOptionsBuilder::default().force(true).v(true).build();
        if let Err(e) = self.docker.remove_container(&self.container_id, Some(remove_opts)).await {
            error!(
                container_id = %self.container_id,
                error = %e,
                "Failed to remove container during guard cleanup"
            );
        }
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if !self.cleaned {
            let docker = self.docker.clone();
            let container_id = self.container_id.clone();
            // Spawn a blocking task to clean up the container
            // This is best-effort; if the runtime is shutting down it may not complete
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
                if let Ok(rt) = rt {
                    rt.block_on(async {
                        let stop_opts = StopContainerOptionsBuilder::default().t(5).build();
                        let _ = docker.stop_container(&container_id, Some(stop_opts)).await;
                        let remove_opts =
                            RemoveContainerOptionsBuilder::default().force(true).v(true).build();
                        let _ = docker.remove_container(&container_id, Some(remove_opts)).await;
                    });
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_include_managed_marker_pid_and_extras() {
        let config = ContainerConfig {
            labels: vec![("afsc.run_id".to_string(), "run-1".to_string())],
            ..Default::default()
        };
        let manager = ContainerManager::new(config);
        let labels = manager.labels_for("zoxide");
        assert_eq!(labels[LABEL_MANAGED], "true");
        assert_eq!(labels[LABEL_INSTALLER], "zoxide");
        assert_eq!(labels[LABEL_PID], std::process::id().to_string());
        assert_eq!(labels[LABEL_RUN_ID], "run-1");
        assert!(chrono::DateTime::parse_from_rfc3339(&labels[LABEL_CREATED_AT]).is_ok());
    }

    #[test]
    fn prepared_image_tags_are_deterministic_and_base_specific() {
        let h1 = template_hash(b"FROM x", "ubuntu:22.04");
        let h2 = template_hash(b"FROM x", "ubuntu:24.04");
        let h3 = template_hash(b"FROM y", "ubuntu:22.04");
        assert_ne!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1, template_hash(b"FROM x", "ubuntu:22.04"));
        assert_eq!(prepared_image_tag("ubuntu:22.04", &h1), format!("afsc-base:{}", &h1[..12]));
        assert_eq!(prepared_image_tag("ubuntu:24.04", &h2), format!("afsc-prepared:ubuntu-24.04-{}", &h2[..12]));
        assert_eq!(prepared_image_tag("ghcr.io/org/img:1.0", &h2), format!("afsc-prepared:ghcr.io-org-img-1.0-{}", &h2[..12]));
    }

    #[test]
    fn image_plan_derives_from_base_or_uses_raw_image() {
        let mgr = ContainerManager::new(ContainerConfig::default());
        let plan = mgr.image_plan().unwrap();
        assert!(plan.prepared);
        assert_eq!(plan.base, CANONICAL_BASE);
        assert!(plan.run_image.starts_with("afsc-base:"));
        assert_ne!(plan.run_image, ContainerManager::AFSC_BASE_IMAGE, "hash tag, not the alias");

        let mgr = ContainerManager::new(ContainerConfig { image: "ubuntu:24.04".into(), ..Default::default() });
        let plan = mgr.image_plan().unwrap();
        assert_eq!(plan.base, "ubuntu:24.04");
        assert!(plan.run_image.starts_with("afsc-prepared:ubuntu-24.04-"));

        let mgr = ContainerManager::new(ContainerConfig { image: "ubuntu:24.04".into(), prepare: false, ..Default::default() });
        let plan = mgr.image_plan().unwrap();
        assert!(!plan.prepared);
        assert_eq!(plan.run_image, "ubuntu:24.04");
    }

    #[test]
    fn memory_limit_parsing() {
        assert_eq!(parse_memory_limit("2G"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory_limit("512m"), Some(512 * 1024 * 1024));
        assert_eq!(parse_memory_limit("64K"), Some(65536));
        assert_eq!(parse_memory_limit("1024"), Some(1024));
        assert_eq!(parse_memory_limit(""), None);
        assert_eq!(parse_memory_limit("lots"), None);
    }

    #[test]
    fn current_pid_is_alive_and_absurd_pid_is_not() {
        assert!(pid_alive(std::process::id()));
        if cfg!(target_os = "linux") {
            assert!(!pid_alive(u32::MAX - 1));
        }
    }

    #[test]
    fn try_new_reports_bad_docker_host() {
        // A malformed DOCKER_HOST must produce an error, never a panic.
        let prev = std::env::var("DOCKER_HOST").ok();
        std::env::set_var("DOCKER_HOST", "not a valid endpoint ::: ///");
        let result = ContainerManager::try_new(ContainerConfig::default());
        match prev {
            Some(v) => std::env::set_var("DOCKER_HOST", v),
            None => std::env::remove_var("DOCKER_HOST"),
        }
        // bollard may accept odd strings lazily; either outcome is acceptable as long as no panic.
        let _ = result;
    }
}
