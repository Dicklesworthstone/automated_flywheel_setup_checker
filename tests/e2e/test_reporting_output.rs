//! Tests for reporting and output functionality

use automated_flywheel_setup_checker::reporting::{
    FailureSummary, JsonlReporter, JsonlWriter, LogEntry, LogLevel, LogRotation, RunSummary,
    SummaryGenerator,
};
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

fn create_temp_dir() -> TempDir {
    TempDir::new().expect("Failed to create temp dir")
}

#[test]
fn test_jsonl_writer_creates_file() {
    let dir = create_temp_dir();
    let path = dir.path().join("test.jsonl");

    let writer = JsonlWriter::new(&path).expect("Should create writer");
    drop(writer); // Close the writer

    assert!(path.exists(), "JSONL file should be created");
}

#[test]
fn test_jsonl_writer_writes_entries() {
    let dir = create_temp_dir();
    let path = dir.path().join("test.jsonl");

    {
        let mut writer = JsonlWriter::new(&path).expect("Should create writer");
        let entry = LogEntry::info("test", "test_event").with_data(serde_json::json!({"key": "value"}));
        writer.write(&entry).expect("Should write entry");
    }

    let content = fs::read_to_string(&path).expect("Should read file");
    assert!(content.contains("test_event"), "Should contain event");
    assert!(content.contains("info"), "Should contain level");
}

#[test]
fn test_log_entry_serialization() {
    let entry = LogEntry::error("runner", "test_failed")
        .with_installer("nodejs")
        .with_error("Connection timeout");

    let json = serde_json::to_string(&entry).expect("Should serialize");
    assert!(json.contains("test_failed"));
    assert!(json.contains("error"));
    assert!(json.contains("nodejs"));
}

#[test]
fn test_log_level_ordering() {
    // Trace < Debug < Info < Warn < Error
    assert!(LogLevel::Trace < LogLevel::Debug);
    assert!(LogLevel::Debug < LogLevel::Info);
    assert!(LogLevel::Info < LogLevel::Warn);
    assert!(LogLevel::Warn < LogLevel::Error);
}

#[test]
fn test_log_rotation_config() {
    let dir = create_temp_dir();
    let rotation = LogRotation::new(dir.path(), 7, "checker");

    assert_eq!(rotation.retention_days(), 7);

    let current_path = rotation.current_log_path();
    assert!(
        current_path.to_string_lossy().contains("checker_"),
        "Path should contain prefix"
    );
    assert!(
        current_path.to_string_lossy().ends_with(".jsonl"),
        "Path should end with .jsonl"
    );
}

#[test]
fn test_jsonl_reporter_filtering() {
    let dir = create_temp_dir();
    let path = dir.path().join("filtered.jsonl");

    {
        let mut reporter = JsonlReporter::new(&path, LogLevel::Warn).expect("Should create reporter");

        // Debug should be filtered out
        reporter.log(LogEntry::debug("test", "debug_event")).expect("Should log");
        // Info should be filtered out
        reporter.log(LogEntry::info("test", "info_event")).expect("Should log");
        // Warn should be included
        reporter.log(LogEntry::warn("test", "warn_event")).expect("Should log");
        // Error should be included
        reporter.log(LogEntry::error("test", "error_event")).expect("Should log");

        reporter.flush().expect("Should flush");
    }

    let content = fs::read_to_string(&path).expect("Should read file");
    assert!(!content.contains("debug_event"), "Debug should be filtered");
    assert!(!content.contains("info_event"), "Info should be filtered");
    assert!(content.contains("warn_event"), "Warn should be included");
    assert!(content.contains("error_event"), "Error should be included");
}

#[test]
fn test_summary_generator_creation() {
    let generator = SummaryGenerator::new("test-run-001");
    // Just verify it can be created
    assert!(true);
    drop(generator);
}

#[test]
fn test_failure_summary_structure() {
    let failure = FailureSummary {
        installer_name: "test-installer".to_string(),
        error_category: "network".to_string(),
        error_message: "Connection timeout".to_string(),
        duration: Duration::from_secs(30),
        retries: 3,
    };

    assert_eq!(failure.installer_name, "test-installer");
    assert_eq!(failure.error_category, "network");
    assert_eq!(failure.retries, 3);
}

#[test]
fn test_multiple_jsonl_entries() {
    let dir = create_temp_dir();
    let path = dir.path().join("multi.jsonl");

    {
        let mut writer = JsonlWriter::new(&path).expect("Should create writer");

        for i in 0..5 {
            let entry = LogEntry::info("test", format!("event_{}", i));
            writer.write(&entry).expect("Should write entry");
        }
    }

    let content = fs::read_to_string(&path).expect("Should read file");
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(lines.len(), 5, "Should have 5 lines");

    for (i, line) in lines.iter().enumerate() {
        // Each line should be valid JSON
        let parsed: serde_json::Value =
            serde_json::from_str(line).expect("Each line should be valid JSON");
        assert!(
            parsed["event"].as_str().unwrap().contains(&format!("event_{}", i)),
            "Should contain correct event"
        );
    }
}

#[test]
fn test_log_entry_with_context() {
    let entry = LogEntry::info("runner", "installer_started")
        .with_installer("nodejs")
        .with_correlation_id("run-12345")
        .with_data(serde_json::json!({"version": "20.0.0"}));

    let json = serde_json::to_string(&entry).expect("Should serialize");
    assert!(json.contains("nodejs"), "Should contain installer");
    assert!(json.contains("run-12345"), "Should contain correlation_id");
    assert!(json.contains("20.0.0"), "Should contain version data");
}

#[test]
fn test_log_entry_with_duration() {
    let entry = LogEntry::info("runner", "test_completed").with_duration_ms(5000);

    let json = serde_json::to_string(&entry).expect("Should serialize");
    assert!(json.contains("5000"), "Should contain duration");
}

#[test]
fn test_log_rotation_list_files() {
    let dir = create_temp_dir();

    // Create some log files
    fs::write(dir.path().join("checker_20260125.jsonl"), "{}").expect("Should write");
    fs::write(dir.path().join("checker_20260126.jsonl"), "{}").expect("Should write");
    fs::write(dir.path().join("checker_20260127.jsonl"), "{}").expect("Should write");
    fs::write(dir.path().join("other_file.txt"), "{}").expect("Should write"); // Should be ignored

    let rotation = LogRotation::new(dir.path(), 7, "checker");
    let files = rotation.list_log_files().expect("Should list files");

    assert_eq!(files.len(), 3, "Should find 3 log files");

    // Should be sorted newest first
    assert!(
        files[0].to_string_lossy().contains("20260127"),
        "First should be newest"
    );
}

#[test]
fn test_log_rotation_prune_old_files() {
    let dir = create_temp_dir();
    let rotation = LogRotation::new(dir.path(), 7, "checker");

    // Create an old log file (simulate 10 days ago)
    let old_date = (chrono::Utc::now() - chrono::Duration::days(10))
        .format("%Y%m%d")
        .to_string();
    let old_file = dir.path().join(format!("checker_{}.jsonl", old_date));
    fs::write(&old_file, "{}").expect("Should write old file");

    // Create a recent log file (today)
    let today = chrono::Utc::now().format("%Y%m%d").to_string();
    let today_file = dir.path().join(format!("checker_{}.jsonl", today));
    fs::write(&today_file, "{}").expect("Should write today file");

    // Prune old files
    let deleted = rotation.prune_old_logs().expect("Should prune");

    assert_eq!(deleted, 1, "Should delete 1 old file");
    assert!(!old_file.exists(), "Old file should be deleted");
    assert!(today_file.exists(), "Today's file should remain");
}

#[test]
fn test_run_summary_serialization() {
    let summary = RunSummary {
        run_id: "test-123".to_string(),
        started_at: chrono::Utc::now(),
        finished_at: chrono::Utc::now(),
        total_duration: Duration::from_secs(120),
        total_tests: 10,
        passed: 8,
        failed: 2,
        skipped: 0,
        timed_out: 0,
        success_rate: 80.0,
        failures: vec![],
    };

    let json = serde_json::to_string(&summary).expect("Should serialize");
    assert!(json.contains("test-123"), "Should contain run_id");
    assert!(json.contains("80"), "Should contain success_rate");
}
