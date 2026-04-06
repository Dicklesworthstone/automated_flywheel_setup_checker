//! E2E tests entry point
//!
//! This file serves as the entry point for E2E integration tests in the `e2e/` directory.
//! These tests verify complete workflows from installer testing through error detection
//! and remediation.

#[path = "e2e/helpers.rs"]
mod helpers;

#[path = "e2e/test_binary_cli.rs"]
mod test_binary_cli;

#[path = "e2e/test_classification_pipeline.rs"]
mod test_classification_pipeline;

#[path = "e2e/test_config_workflow.rs"]
mod test_config_workflow;

#[path = "e2e/test_reporting_output.rs"]
mod test_reporting_output;

#[path = "e2e/test_docker_integration.rs"]
mod test_docker_integration;
