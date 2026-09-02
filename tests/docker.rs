//! Docker integration tests (real containers). Gated on `AFSC_DOCKER_TESTS=1`; each test skips
//! with a message otherwise so `cargo test` stays green on hosts without Docker.
//!
//! Run locally:  AFSC_DOCKER_TESTS=1 cargo test --test docker -- --test-threads=1

#[path = "docker/support.rs"]
pub mod support;

#[path = "docker/lifecycle.rs"]
mod lifecycle;
