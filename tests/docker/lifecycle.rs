//! Container lifecycle: create/exec/cleanup, limits, labels, non-root, reaper, cancellation.

use super::support::*;
use crate::skip_unless_docker;
use automated_flywheel_setup_checker::runner::{
    ChecksumState, ContainerConfig, ContainerManager, InstallerTestRunner, TestStatus,
};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn preflight_succeeds_against_a_live_daemon() {
    skip_unless_docker!();
    let manager = ContainerManager::try_new(ContainerConfig::default()).unwrap();
    manager.preflight().await.expect("daemon reachable");
}

#[tokio::test]
async fn create_exec_and_cleanup_with_labels_limits_and_env() {
    skip_unless_docker!();
    let fx = DockerFixture::new();
    let mut config = fx.container_config();
    config.memory_limit = Some(256 * 1024 * 1024);
    config.environment.push(("TEST_VAR".into(), "test_value".into()));
    config.labels.push(("afsc.run_id".into(), "run-docker-test".into()));
    let manager = ContainerManager::try_new(config).unwrap();

    let id = manager.create_container("lifecycle").await.unwrap();
    assert!(!id.is_empty());

    // Naming convention and labels
    let name = docker_inspect(&id, "{{.Name}}");
    assert!(name.trim_start_matches('/').starts_with("afsc-lifecycle-"), "{name}");
    assert_eq!(docker_inspect(&id, "{{index .Config.Labels \"afsc.managed\"}}"), "true");
    assert_eq!(docker_inspect(&id, "{{index .Config.Labels \"afsc.installer\"}}"), "lifecycle");
    assert_eq!(docker_inspect(&id, "{{index .Config.Labels \"afsc.run_id\"}}"), "run-docker-test");
    assert_eq!(
        docker_inspect(&id, "{{index .Config.Labels \"afsc.pid\"}}"),
        std::process::id().to_string()
    );
    // Resource limit visible via inspect
    assert_eq!(docker_inspect(&id, "{{.HostConfig.Memory}}"), (256 * 1024 * 1024).to_string());

    // Exec: env vars, non-root user, exit codes
    let (code, out, _) = manager.exec_in_container(&id, &["bash", "-c", "echo $TEST_VAR"]).await.unwrap();
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "test_value");
    let (_, out, _) = manager.exec_in_container(&id, &["bash", "-c", "echo $DEBIAN_FRONTEND"]).await.unwrap();
    assert_eq!(out.trim(), "noninteractive");
    let (_, out, _) = manager.exec_in_container(&id, &["id", "-un"]).await.unwrap();
    assert_eq!(out.trim(), "afsc-user", "default image runs as the non-root user");
    let (code, _, _) = manager.exec_in_container(&id, &["bash", "-c", "exit 42"]).await.unwrap();
    assert_eq!(code, 42);
    // stdout and stderr are separated (no tty on exec)
    let (_, out, err) =
        manager.exec_in_container(&id, &["bash", "-c", "echo out; echo err >&2"]).await.unwrap();
    assert_eq!(out.trim(), "out");
    assert_eq!(err.trim(), "err");

    manager.cleanup_container(&id).await.unwrap();
    // Idempotent and tolerant of unknown ids
    manager.cleanup_container(&id).await.unwrap();
    manager.cleanup_container("nonexistent-container-12345").await.unwrap();
    assert!(docker_ps_names("label=afsc.run_id=run-docker-test").is_empty());
}

#[tokio::test]
async fn real_run_verifies_checksum_and_refuses_mismatch() {
    skip_unless_docker!();
    let fx = DockerFixture::new();
    let (url, sha) = fx.add_installer(
        "pass",
        "#!/bin/bash\necho \"ran as $(id -un) HOME=$HOME\"\ntouch \"$HOME/.afsc-ran\"\nexit 0\n",
    );
    let (bad_url, _) = fx.add_installer("bad", "#!/bin/bash\ntouch /tmp/afsc-must-not-exist\nexit 0\n");
    let runner = InstallerTestRunner::new(fx.runner_config(Duration::from_secs(120)));

    let ok = runner.run_test_with_retry(&fx.test("pass", &url, &sha, Duration::from_secs(120))).await.unwrap();
    assert_eq!(ok.status, TestStatus::Passed, "stderr: {}", ok.stderr);
    assert_eq!(ok.checksum_state, ChecksumState::Verified);
    assert!(ok.stdout.contains("ran as afsc-user"), "{}", ok.stdout);
    assert!(ok.container_id.is_some());

    let wrong = fx.test("bad", &bad_url, &"0".repeat(64), Duration::from_secs(120));
    let refused = runner.run_test_with_retry(&wrong).await.unwrap();
    assert_eq!(refused.status, TestStatus::Failed);
    assert_eq!(refused.exit_code, Some(99));
    assert_eq!(refused.checksum_state, ChecksumState::Mismatch);
    assert_eq!(refused.error.as_ref().unwrap().category, "checksum_mismatch");
    assert_eq!(refused.attempts.len(), 1, "mismatch is never retried");
    assert!(!refused.stdout.contains("must-not-exist"));

    // Verified-but-failed: checksum state stays Verified and the failure is classified.
    let (fail_url, fail_sha) =
        fx.add_installer("dep", "#!/bin/bash\necho 'E: Unable to locate package foo' >&2\nexit 100\n");
    let failed = runner.run_test_with_retry(&fx.test("dep", &fail_url, &fail_sha, Duration::from_secs(120))).await.unwrap();
    assert_eq!(failed.status, TestStatus::Failed);
    assert_eq!(failed.checksum_state, ChecksumState::Verified);
    assert_eq!(failed.error.as_ref().unwrap().category, "dependency");

    assert!(docker_ps_names("label=afsc.installer=pass").is_empty(), "containers cleaned up");
    assert!(docker_ps_names("label=afsc.installer=dep").is_empty());
}

#[tokio::test]
async fn timeout_kills_the_installer_and_removes_the_container() {
    skip_unless_docker!();
    let fx = DockerFixture::new();
    let (url, sha) = fx.add_installer("sleeper", "#!/bin/bash\nsleep 120\nexit 0\n");
    let runner = InstallerTestRunner::new(fx.runner_config(Duration::from_secs(5)));
    let start = Instant::now();
    let r = runner.run_test_with_retry(&fx.test("sleeper", &url, &sha, Duration::from_secs(5))).await.unwrap();
    assert_eq!(r.status, TestStatus::TimedOut);
    assert_eq!(r.error.as_ref().unwrap().category, "timeout");
    assert!(start.elapsed() < Duration::from_secs(40), "{:?}", start.elapsed());
    assert!(docker_ps_names("label=afsc.installer=sleeper").is_empty(), "container removed after timeout");
}

#[tokio::test]
async fn cancellation_stops_the_installer_and_removes_the_container() {
    skip_unless_docker!();
    let fx = DockerFixture::new();
    let (url, sha) = fx.add_installer("cancelme", "#!/bin/bash\nsleep 120\nexit 0\n");
    let cancel = CancellationToken::new();
    let mut config = fx.runner_config(Duration::from_secs(300));
    config.cancel = cancel.clone();
    let runner = InstallerTestRunner::new(config);
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(4)).await;
        canceller.cancel();
    });
    let start = Instant::now();
    let r = runner.run_test_with_retry(&fx.test("cancelme", &url, &sha, Duration::from_secs(300))).await.unwrap();
    assert_eq!(r.status, TestStatus::Cancelled, "stderr: {}", r.stderr);
    assert_eq!(r.error.as_ref().unwrap().category, "cancelled");
    assert!(start.elapsed() < Duration::from_secs(30), "{:?}", start.elapsed());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(docker_ps_names("label=afsc.installer=cancelme").is_empty(), "container removed after cancel");
}

#[tokio::test]
async fn reaper_removes_dead_owner_containers_and_leaves_others_alone() {
    skip_unless_docker!();
    let fx = DockerFixture::new();
    // A labeled container whose "owner" pid is dead.
    let mut orphan_cfg = fx.container_config();
    orphan_cfg.labels.push(("afsc.pid".into(), (u32::MAX - 7).to_string()));
    orphan_cfg.labels.push(("afsc.run_id".into(), "reaper-test".into()));
    let orphan_mgr = ContainerManager::try_new(orphan_cfg).unwrap();
    let orphan_id = orphan_mgr.create_container("orphan").await.unwrap();

    // An unlabeled bystander that must survive.
    let bystander = format!("afsc-bystander-{}", std::process::id());
    docker_rm_force(&bystander);
    let status = std::process::Command::new("docker")
        .args(["run", "-d", "--name", &bystander, "ubuntu:22.04", "sleep", "60"])
        .status()
        .unwrap();
    assert!(status.success());

    // A container owned by this process must also survive.
    let mine_mgr = ContainerManager::try_new(fx.container_config()).unwrap();
    let mine_id = mine_mgr.create_container("mine").await.unwrap();

    let reaper = ContainerManager::try_new(ContainerConfig::default()).unwrap();
    let reaped = reaper.reap_orphans(Duration::from_secs(3600)).await.unwrap();
    let names: Vec<&str> = reaped.iter().map(|c| c.name.as_str()).collect();
    assert!(names.iter().any(|n| n.starts_with("afsc-orphan-")), "{names:?}");
    assert!(!names.iter().any(|n| n.starts_with("afsc-mine-")), "{names:?}");
    assert!(docker_ps_names("label=afsc.run_id=reaper-test").is_empty());
    assert_eq!(docker_ps_names(&format!("name={bystander}")).len(), 1, "bystander untouched");
    assert_eq!(docker_inspect(&mine_id, "{{.State.Running}}"), "true");

    // Age-based reaping: even a live owner loses containers older than max_age.
    let reaped = reaper.reap_orphans(Duration::from_millis(1)).await.unwrap();
    // Our own containers are never reaped by the same process; nothing else remains.
    assert!(reaped.iter().all(|c| !c.name.starts_with("afsc-mine-")));

    mine_mgr.cleanup_container(&mine_id).await.unwrap();
    let _ = orphan_mgr.cleanup_container(&orphan_id).await;
    docker_rm_force(&bystander);
}
