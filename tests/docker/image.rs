//! Prepared-image lifecycle: derivation from any base, caching, non-root user, run_as_root.

use super::support::*;
use crate::skip_unless_docker;
use automated_flywheel_setup_checker::runner::ContainerManager;
use std::time::Instant;

fn manager(image: &str, run_as_root: bool, fx: &DockerFixture) -> ContainerManager {
    let mut cfg = fx.container_config();
    cfg.image = image.to_string();
    cfg.run_as_root = run_as_root;
    cfg.labels.push(("afsc.run_id".into(), "image-test".into()));
    ContainerManager::try_new(cfg).unwrap()
}

#[tokio::test]
async fn canonical_base_runs_as_non_root_and_keeps_the_latest_alias() {
    skip_unless_docker!();
    let fx = DockerFixture::new();
    let m = manager(ContainerManager::AFSC_BASE_IMAGE, false, &fx);
    let plan = m.image_plan().unwrap();
    assert!(plan.prepared);
    assert!(plan.run_image.starts_with("afsc-base:"));
    let id = must(m.create_container("canon").await, "create_container");
    let (_, user, _) = must(m.exec_in_container(&id, &["id", "-un"]).await, "exec_in_container");
    assert_eq!(user.trim(), "afsc-user");
    assert_eq!(docker_inspect(&id, "{{.Config.Image}}"), plan.run_image);
    must(m.cleanup_container(&id).await, "cleanup_container");
    // The alias exists and points at the hash-tagged build.
    let alias = docker_inspect(ContainerManager::AFSC_BASE_IMAGE, "{{.Id}}");
    let hashed = docker_inspect(&plan.run_image, "{{.Id}}");
    assert_eq!(alias, hashed, "afsc-base:latest must alias the current hash tag");
}

#[tokio::test]
async fn foreign_base_is_prepared_once_and_cached() {
    skip_unless_docker!();
    let fx = DockerFixture::new();
    let m = manager("ubuntu:22.04", false, &fx);
    let plan = m.image_plan().unwrap();
    assert_eq!(plan.base, "ubuntu:22.04");
    assert!(plan.run_image.starts_with("afsc-prepared:ubuntu-22.04-"));

    // First run may build (minutes); the container must still run as the non-root user.
    let id = must(m.create_container("prep1").await, "create_container");
    let (_, user, _) = must(m.exec_in_container(&id, &["id", "-un"]).await, "exec_in_container");
    assert_eq!(user.trim(), "afsc-user");
    let (_, rel, _) = must(m.exec_in_container(&id, &["bash", "-c", "lsb_release -rs 2>/dev/null || cat /etc/os-release"]).await, "exec_in_container");
    assert!(rel.contains("22.04"), "{rel}");
    let (code, _, _) = must(m.exec_in_container(&id, &["bash", "-lc", "command -v cargo && command -v node && command -v jq"]).await, "exec_in_container");
    assert_eq!(code, 0, "prerequisites present on the derived image");
    must(m.cleanup_container(&id).await, "cleanup_container");

    // Second run reuses the cached image.
    let start = Instant::now();
    let id = must(m.create_container("prep2").await, "create_container");
    assert!(start.elapsed().as_secs() < 20, "cached prepared image: {:?}", start.elapsed());
    must(m.cleanup_container(&id).await, "cleanup_container");
}

#[tokio::test]
async fn run_as_root_and_prepare_false_run_as_root() {
    skip_unless_docker!();
    let fx = DockerFixture::new();
    let m = manager(ContainerManager::AFSC_BASE_IMAGE, true, &fx);
    let id = must(m.create_container("root1").await, "create_container");
    let (_, user, _) = must(m.exec_in_container(&id, &["id", "-un"]).await, "exec_in_container");
    assert_eq!(user.trim(), "root");
    must(m.cleanup_container(&id).await, "cleanup_container");

    let mut cfg = fx.container_config();
    cfg.image = "ubuntu:22.04".into();
    cfg.prepare = false;
    let m = ContainerManager::try_new(cfg).unwrap();
    let plan = m.image_plan().unwrap();
    assert!(!plan.prepared);
    assert_eq!(plan.run_image, "ubuntu:22.04");
    let id = must(m.create_container("raw").await, "create_container");
    let (_, user, _) = must(m.exec_in_container(&id, &["id", "-un"]).await, "exec_in_container");
    assert_eq!(user.trim(), "root");
    must(m.cleanup_container(&id).await, "cleanup_container");
}
