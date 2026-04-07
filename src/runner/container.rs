//! Docker container management via Bollard
//!
//! Provides real Docker container lifecycle operations: create, exec, cleanup.
//! Uses bollard 0.16 to communicate with the Docker daemon.

use anyhow::{Context, Result};
use bollard::container::{
    Config, CreateContainerOptions, RemoveContainerOptions, StopContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use bollard::Docker;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Configuration for Docker containers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    pub image: String,
    pub memory_limit: Option<u64>,
    pub cpu_quota: Option<f64>,
    pub timeout_seconds: u64,
    pub volumes: Vec<(String, String)>,
    pub environment: Vec<(String, String)>,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            image: "ubuntu:22.04".to_string(),
            memory_limit: Some(2 * 1024 * 1024 * 1024), // 2GB
            cpu_quota: Some(1.0),
            timeout_seconds: 300,
            volumes: Vec::new(),
            environment: Vec::new(),
        }
    }
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

/// Manages Docker containers for installer testing
pub struct ContainerManager {
    config: ContainerConfig,
    docker: Arc<Docker>,
    pull_policy: PullPolicy,
}

impl ContainerManager {
    /// Create a new ContainerManager connected to the local Docker daemon
    pub fn new(config: ContainerConfig) -> Self {
        let docker =
            Docker::connect_with_local_defaults().expect("Failed to connect to Docker daemon");
        Self { config, docker: Arc::new(docker), pull_policy: PullPolicy::IfNotPresent }
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

    /// Pull the Docker image if required by the pull policy
    async fn ensure_image(&self) -> Result<()> {
        let image = &self.config.image;

        match self.pull_policy {
            PullPolicy::Never => {
                debug!(image = %image, "Pull policy is Never, skipping pull");
                return Ok(());
            }
            PullPolicy::IfNotPresent => {
                // Check if image exists locally
                match self.docker.inspect_image(image).await {
                    Ok(_) => {
                        debug!(image = %image, "Image already present locally");
                        return Ok(());
                    }
                    Err(_) => {
                        info!(image = %image, "Image not found locally, pulling");
                    }
                }
            }
            PullPolicy::Always => {
                info!(image = %image, "Pull policy is Always, pulling image");
            }
        }

        // Split image into repo and tag
        let (repo, tag) = if let Some(pos) = image.rfind(':') {
            (&image[..pos], &image[pos + 1..])
        } else {
            (image.as_str(), "latest")
        };

        let opts = CreateImageOptions { from_image: repo, tag, ..Default::default() };

        let mut stream = self.docker.create_image(Some(opts), None, None);
        while let Some(result) = stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(status) = info.status {
                        debug!(status = %status, "Pull progress");
                    }
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Failed to pull image {}: {}", image, e));
                }
            }
        }

        info!(image = %image, "Image pulled successfully");
        Ok(())
    }

    /// Create and start a container for testing
    ///
    /// Returns the container ID string from Docker.
    pub async fn create_container(&self, name: &str) -> Result<String> {
        // Ensure image is available
        self.ensure_image().await.context("Failed to ensure Docker image")?;

        // Build container name: afsc-INSTALLERNAME-TIMESTAMP-RANDOM
        // Include milliseconds and random suffix to avoid collisions in parallel mode
        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f");
        let random_suffix: u16 = rand::random();
        let container_name = format!("afsc-{}-{}-{:04x}", name, timestamp, random_suffix);

        // Build environment variables
        let mut env: Vec<String> = vec![
            "DEBIAN_FRONTEND=noninteractive".to_string(),
            "HOME=/root".to_string(),
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
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

        // Volume binds
        if !self.config.volumes.is_empty() {
            let binds: Vec<String> = self
                .config
                .volumes
                .iter()
                .map(|(host, container)| format!("{}:{}", host, container))
                .collect();
            host_config.binds = Some(binds);
        }

        // Create container config
        let container_config = Config {
            image: Some(self.config.image.clone()),
            env: Some(env),
            host_config: Some(host_config),
            // Keep container alive with a long sleep so we can exec into it
            cmd: Some(vec!["sleep".to_string(), "86400".to_string()]),
            working_dir: Some("/root".to_string()),
            ..Default::default()
        };

        let create_opts = CreateContainerOptions { name: container_name.as_str(), platform: None };

        let response = self
            .docker
            .create_container(Some(create_opts), container_config)
            .await
            .context("Failed to create Docker container")?;

        let container_id = response.id.clone();
        info!(
            container_id = %container_id,
            container_name = %container_name,
            image = %self.config.image,
            "Container created"
        );

        // Start the container
        self.docker
            .start_container::<String>(&container_id, None)
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
        debug!(
            container_id = %container_id,
            command = ?command,
            "Executing command in container"
        );

        let exec_opts = CreateExecOptions {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
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
            StartExecResults::Attached { mut output, .. } => {
                while let Some(msg) = output.next().await {
                    match msg {
                        Ok(bollard::container::LogOutput::StdOut { message }) => {
                            stdout_buf.extend_from_slice(&message);
                        }
                        Ok(bollard::container::LogOutput::StdErr { message }) => {
                            stderr_buf.extend_from_slice(&message);
                        }
                        Ok(_) => {} // Console or other log types
                        Err(e) => {
                            warn!(error = %e, "Error reading exec output");
                            break;
                        }
                    }
                }
            }
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
        let stop_opts = StopContainerOptions { t: 10 };
        if let Err(e) = self.docker.stop_container(container_id, Some(stop_opts)).await {
            // 304 = already stopped, 404 = not found — both are fine
            debug!(
                container_id = %container_id,
                error = %e,
                "Stop container returned error (may already be stopped)"
            );
        }

        // Force remove
        let remove_opts = RemoveContainerOptions { force: true, v: true, ..Default::default() };
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

        let stop_opts = StopContainerOptions { t: 10 };
        if let Err(e) = self.docker.stop_container(&self.container_id, Some(stop_opts)).await {
            debug!(
                container_id = %self.container_id,
                error = %e,
                "Stop during guard cleanup (may already be stopped)"
            );
        }

        let remove_opts = RemoveContainerOptions { force: true, v: true, ..Default::default() };
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
                        let stop_opts = StopContainerOptions { t: 5 };
                        let _ = docker.stop_container(&container_id, Some(stop_opts)).await;
                        let remove_opts =
                            RemoveContainerOptions { force: true, v: true, ..Default::default() };
                        let _ = docker.remove_container(&container_id, Some(remove_opts)).await;
                    });
                }
            });
        }
    }
}
