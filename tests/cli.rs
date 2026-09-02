//! CLI integration tests: drive the real binary against synthetic fixtures (no Docker).
//!
//! Each test prints the command line, stdout, and stderr on failure so CI logs are diagnosable.

#[path = "cli/support.rs"]
mod support;

#[path = "cli/check.rs"]
mod check;

#[path = "cli/status.rs"]
mod status;

#[path = "cli/misc.rs"]
mod misc;

#[path = "cli/spec.rs"]
mod spec;

#[path = "cli/history.rs"]
mod history;

#[path = "cli/serve.rs"]
mod serve;

#[path = "cli/notify.rs"]
mod notify;

#[path = "cli/doctor.rs"]
mod doctor;
