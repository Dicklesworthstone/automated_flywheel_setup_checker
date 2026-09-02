//! Unit tests entry point
//!
//! This file serves as the entry point for integration tests in the `unit/` directory.
//! Cargo automatically discovers `.rs` files in `tests/` and runs them as integration tests.

#[path = "unit/checksums_tests.rs"]
mod checksums_tests;

#[path = "unit/config_tests.rs"]
mod config_tests;

#[path = "unit/parser_tests.rs"]
mod parser_tests;

#[path = "unit/remediation_tests.rs"]
mod remediation_tests;

#[path = "unit/reporting_tests.rs"]
mod reporting_tests;

#[path = "unit/resolve_tests.rs"]
mod resolve_tests;

#[path = "unit/runner_tests.rs"]
mod runner_tests;
