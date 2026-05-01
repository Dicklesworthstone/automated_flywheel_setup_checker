//! Tests for the reporting module
//!
//! Tests cover:
//! - JSONL writing and formatting
//! - Log entry creation and builder pattern
//! - Log level filtering
//! - Summary generation from test results

use automated_flywheel_setup_checker::reporting::{
    JsonlReporter, JsonlWriter, LogEntry, LogLevel, ResultPersister, SummaryGenerator,
};
use automated_flywheel_setup_checker::runner::TestResult;
use serde::{Deserialize, Serialize};
use std::fs;
use std::time::Duration;
use tempfile::NamedTempFile;

// ============================================================================
// LogLevel Tests
// ============================================================================

#[test]
fn test_log_level_ordering() {
    // Levels should be ordered from least to most severe
    assert!(LogLevel::Trace < LogLevel::Debug);
    assert!(LogLevel::Debug < LogLevel::Info);
    assert!(LogLevel::Info < LogLevel::Warn);
    assert!(LogLevel::Warn < LogLevel::Error);
}

#[test]
fn test_log_level_equality() {
    assert_eq!(LogLevel::Info, LogLevel::Info);
    assert_ne!(LogLevel::Info, LogLevel::Debug);
}

#[test]
fn test_log_level_default() {
    let level = LogLevel::default();
    assert_eq!(level, LogLevel::Info);
}

#[test]
fn test_log_level_copy() {
    let level = LogLevel::Warn;
    let copied = level;
    assert_eq!(level, copied);
}

// ============================================================================
// LogEntry Tests
// ============================================================================

#[test]
fn test_log_entry_new() {
    let entry = LogEntry::new(LogLevel::Info, "runner", "test_started");
    assert_eq!(entry.level, LogLevel::Info);
    assert_eq!(entry.component, "runner");
    assert_eq!(entry.event, "test_started");
    assert!(entry.duration_ms.is_none());
    assert!(entry.error.is_none());
    assert!(entry.correlation_id.is_none());
    assert!(entry.installer.is_none());
}

#[test]
fn test_log_entry_info() {
    let entry = LogEntry::info("component", "event");
    assert_eq!(entry.level, LogLevel::Info);
}

#[test]
fn test_log_entry_error() {
    let entry = LogEntry::error("component", "event");
    assert_eq!(entry.level, LogLevel::Error);
}

#[test]
fn test_log_entry_warn() {
    let entry = LogEntry::warn("component", "event");
    assert_eq!(entry.level, LogLevel::Warn);
}

#[test]
fn test_log_entry_debug() {
    let entry = LogEntry::debug("component", "event");
    assert_eq!(entry.level, LogLevel::Debug);
}

#[test]
fn test_log_entry_with_data() {
    let entry =
        LogEntry::info("runner", "test").with_data(serde_json::json!({"key": "value", "num": 42}));
    assert!(!entry.data.is_null());
}

#[test]
fn test_log_entry_with_duration_ms() {
    let entry = LogEntry::info("runner", "test").with_duration_ms(1500);
    assert_eq!(entry.duration_ms, Some(1500));
}

#[test]
fn test_log_entry_with_error() {
    let entry = LogEntry::error("runner", "test_failed").with_error("Something went wrong");
    assert_eq!(entry.error, Some("Something went wrong".to_string()));
}

#[test]
fn test_log_entry_with_correlation_id() {
    let entry = LogEntry::info("runner", "test").with_correlation_id("run-12345");
    assert_eq!(entry.correlation_id, Some("run-12345".to_string()));
}

#[test]
fn test_log_entry_with_installer() {
    let entry = LogEntry::info("runner", "test_started").with_installer("nodejs");
    assert_eq!(entry.installer, Some("nodejs".to_string()));
}

#[test]
fn test_log_entry_builder_chain() {
    let entry = LogEntry::info("runner", "test_started")
        .with_installer("nodejs")
        .with_correlation_id("run-123")
        .with_duration_ms(5000)
        .with_data(serde_json::json!({"version": "20.0"}));

    assert_eq!(entry.level, LogLevel::Info);
    assert_eq!(entry.component, "runner");
    assert_eq!(entry.event, "test_started");
    assert_eq!(entry.installer, Some("nodejs".to_string()));
    assert_eq!(entry.correlation_id, Some("run-123".to_string()));
    assert_eq!(entry.duration_ms, Some(5000));
}

#[test]
fn test_log_entry_timestamp() {
    let entry = LogEntry::info("test", "event");
    // Timestamp should be set to current time
    let now = chrono::Utc::now();
    assert!(entry.timestamp <= now);
}

// ============================================================================
// JsonlWriter Tests
// ============================================================================

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct TestRecord {
    name: String,
    value: i32,
}

#[test]
fn test_jsonl_writer_basic() {
    let file = NamedTempFile::new().unwrap();
    let mut writer = JsonlWriter::new(file.path()).unwrap();

    let record = TestRecord { name: "test".to_string(), value: 42 };
    writer.write(&record).unwrap();

    let content = fs::read_to_string(file.path()).unwrap();
    assert!(content.contains("\"name\":\"test\""));
    assert!(content.contains("\"value\":42"));
}

#[test]
fn test_jsonl_writer_multiple_records() {
    let file = NamedTempFile::new().unwrap();
    let mut writer = JsonlWriter::new(file.path()).unwrap();

    for i in 0..3 {
        let record = TestRecord { name: format!("test{}", i), value: i };
        writer.write(&record).unwrap();
    }

    let content = fs::read_to_string(file.path()).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 3);
}

#[test]
fn test_jsonl_writer_flush() {
    let file = NamedTempFile::new().unwrap();
    let mut writer = JsonlWriter::new(file.path()).unwrap();

    let record = TestRecord { name: "test".to_string(), value: 1 };
    writer.write(&record).unwrap();
    writer.flush().unwrap();

    let content = fs::read_to_string(file.path()).unwrap();
    assert!(!content.is_empty());
}

// ============================================================================
// JsonlReporter Tests
// ============================================================================

#[test]
fn test_jsonl_reporter_basic() {
    let file = NamedTempFile::new().unwrap();
    let mut reporter = JsonlReporter::new(file.path(), LogLevel::Info).unwrap();

    reporter.log(LogEntry::info("test", "event")).unwrap();
    reporter.flush().unwrap();

    let content = fs::read_to_string(file.path()).unwrap();
    assert!(content.contains("\"event\":\"event\""));
}

#[test]
fn test_jsonl_reporter_filtering() {
    let file = NamedTempFile::new().unwrap();
    let mut reporter = JsonlReporter::new(file.path(), LogLevel::Warn).unwrap();

    // Debug should be filtered out
    reporter.log(LogEntry::debug("test", "debug_event")).unwrap();
    // Info should be filtered out
    reporter.log(LogEntry::info("test", "info_event")).unwrap();
    // Warn should be included
    reporter.log(LogEntry::warn("test", "warn_event")).unwrap();
    // Error should be included
    reporter.log(LogEntry::error("test", "error_event")).unwrap();

    reporter.flush().unwrap();

    let content = fs::read_to_string(file.path()).unwrap();
    assert!(!content.contains("debug_event"));
    assert!(!content.contains("info_event"));
    assert!(content.contains("warn_event"));
    assert!(content.contains("error_event"));
}

#[test]
fn test_jsonl_reporter_min_level() {
    let file = NamedTempFile::new().unwrap();
    let reporter = JsonlReporter::new(file.path(), LogLevel::Error).unwrap();
    assert_eq!(reporter.min_level(), LogLevel::Error);
}

#[test]
fn test_jsonl_reporter_with_fsync() {
    let file = NamedTempFile::new().unwrap();
    let reporter = JsonlReporter::new(file.path(), LogLevel::Info).unwrap().with_fsync(true);
    // Just verify it can be called - actual fsync behavior is hard to test
    drop(reporter);
}

#[test]
fn test_jsonl_reporter_with_buffer_size() {
    let file = NamedTempFile::new().unwrap();
    let mut reporter = JsonlReporter::new(file.path(), LogLevel::Info).unwrap().with_buffer_size(2);

    // Add entries, should auto-flush at buffer size
    reporter.log(LogEntry::info("test", "event1")).unwrap();
    reporter.log(LogEntry::info("test", "event2")).unwrap();
    // Buffer should have been flushed

    let content = fs::read_to_string(file.path()).unwrap();
    assert!(content.contains("event1"));
    assert!(content.contains("event2"));
}

#[test]
fn test_jsonl_reporter_log_if_true() {
    let file = NamedTempFile::new().unwrap();
    let mut reporter = JsonlReporter::new(file.path(), LogLevel::Info).unwrap();

    reporter.log_if(true, LogEntry::info("test", "included")).unwrap();
    reporter.log_if(false, LogEntry::info("test", "excluded")).unwrap();
    reporter.flush().unwrap();

    let content = fs::read_to_string(file.path()).unwrap();
    assert!(content.contains("included"));
    assert!(!content.contains("excluded"));
}

#[test]
fn test_jsonl_reporter_log_batch() {
    let file = NamedTempFile::new().unwrap();
    let mut reporter = JsonlReporter::new(file.path(), LogLevel::Info).unwrap();

    let entries = vec![
        LogEntry::info("test", "batch1"),
        LogEntry::info("test", "batch2"),
        LogEntry::info("test", "batch3"),
    ];

    reporter.log_batch(entries).unwrap();
    reporter.flush().unwrap();

    let content = fs::read_to_string(file.path()).unwrap();
    assert!(content.contains("batch1"));
    assert!(content.contains("batch2"));
    assert!(content.contains("batch3"));
}

// ============================================================================
// SummaryGenerator Tests
// ============================================================================

#[test]
fn test_summary_generator_new() {
    let generator = SummaryGenerator::new("test-run-1");
    // Just verify it can be created
    let _ = generator;
}

#[test]
fn test_summary_generation_all_passed() {
    let generator = SummaryGenerator::new("test-run-1");

    let results = vec![
        TestResult::new("installer1").passed(),
        TestResult::new("installer2").passed(),
        TestResult::new("installer3").passed(),
    ];

    let summary = generator.generate(&results);

    assert_eq!(summary.run_id, "test-run-1");
    assert_eq!(summary.total_tests, 3);
    assert_eq!(summary.passed, 3);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.skipped, 0);
    assert_eq!(summary.timed_out, 0);
    assert_eq!(summary.success_rate, 100.0);
    assert!(summary.failures.is_empty());
}

#[test]
fn test_summary_generation_mixed_results() {
    let generator = SummaryGenerator::new("test-run-2");

    let results = vec![
        TestResult::new("installer1").passed(),
        TestResult::new("installer2").passed(),
        TestResult::new("installer3").failed(1, "error"),
    ];

    let summary = generator.generate(&results);

    assert_eq!(summary.total_tests, 3);
    assert_eq!(summary.passed, 2);
    assert_eq!(summary.failed, 1);
    assert!((summary.success_rate - 66.666).abs() < 1.0);
    assert_eq!(summary.failures.len(), 1);
    assert_eq!(summary.failures[0].installer_name, "installer3");
}

#[test]
fn test_summary_generation_all_failed() {
    let generator = SummaryGenerator::new("test-run-3");

    let results = vec![
        TestResult::new("installer1").failed(1, "error1"),
        TestResult::new("installer2").failed(2, "error2"),
    ];

    let summary = generator.generate(&results);

    assert_eq!(summary.total_tests, 2);
    assert_eq!(summary.passed, 0);
    assert_eq!(summary.failed, 2);
    assert_eq!(summary.success_rate, 0.0);
}

#[test]
fn test_summary_generation_with_timeouts() {
    let generator = SummaryGenerator::new("test-run-4");

    let results =
        vec![TestResult::new("installer1").passed(), TestResult::new("installer2").timed_out()];

    let summary = generator.generate(&results);

    assert_eq!(summary.total_tests, 2);
    assert_eq!(summary.passed, 1);
    assert_eq!(summary.timed_out, 1);
    assert_eq!(summary.failures.len(), 1); // Timeouts count as failures
}

#[test]
fn test_summary_generation_with_skipped() {
    let generator = SummaryGenerator::new("test-run-5");

    let results = vec![
        TestResult::new("installer1").passed(),
        TestResult::new("installer2").skipped("disabled"),
    ];

    let summary = generator.generate(&results);

    assert_eq!(summary.total_tests, 2);
    assert_eq!(summary.passed, 1);
    assert_eq!(summary.skipped, 1);
    // Skipped tests don't affect success rate calculation in the same way
}

#[test]
fn test_summary_generation_empty() {
    let generator = SummaryGenerator::new("empty-run");
    let results: Vec<TestResult> = vec![];

    let summary = generator.generate(&results);

    assert_eq!(summary.total_tests, 0);
    assert_eq!(summary.passed, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.success_rate, 0.0);
}

#[test]
fn test_summary_timestamps() {
    let generator = SummaryGenerator::new("test-run");
    let results = vec![TestResult::new("test").passed()];

    let summary = generator.generate(&results);

    assert!(summary.started_at <= summary.finished_at);
}

#[test]
fn test_summary_duration() {
    let generator = SummaryGenerator::new("test-run");
    std::thread::sleep(Duration::from_millis(10));
    let results = vec![TestResult::new("test").passed()];

    let summary = generator.generate(&results);

    assert!(summary.total_duration.as_millis() >= 10);
}

// ============================================================================
// FailureSummary Tests
// ============================================================================

#[test]
fn test_failure_summary_from_failed_result() {
    let generator = SummaryGenerator::new("test-run");

    let mut result = TestResult::new("failed-installer");
    result.add_retry("first failure", 1000);
    let result = result.failed(1, "Installation failed: missing dependency");

    let summary = generator.generate(&[result]);

    assert_eq!(summary.failures.len(), 1);
    let failure = &summary.failures[0];
    assert_eq!(failure.installer_name, "failed-installer");
    assert_eq!(failure.error_message, "Installation failed: missing dependency");
    assert_eq!(failure.retries, 1);
}

// ============================================================================
// Serialization Tests
// ============================================================================

#[test]
fn test_log_entry_serializable() {
    let entry = LogEntry::info("runner", "test")
        .with_installer("nodejs")
        .with_duration_ms(1000)
        .with_data(serde_json::json!({"key": "value"}));

    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains("\"level\":\"info\""));
    assert!(json.contains("\"component\":\"runner\""));
    assert!(json.contains("\"installer\":\"nodejs\""));
}

#[test]
fn test_log_level_serializable() {
    let level = LogLevel::Warn;
    let json = serde_json::to_string(&level).unwrap();
    assert_eq!(json, "\"warn\"");
}

#[test]
fn test_run_summary_serializable() {
    let generator = SummaryGenerator::new("test-run");
    let results = vec![TestResult::new("test").passed()];
    let summary = generator.generate(&results);

    let json = serde_json::to_string(&summary).unwrap();
    assert!(json.contains("\"run_id\":\"test-run\""));
    assert!(json.contains("\"total_tests\":1"));
}

// ============================================================================
// ResultPersister Tests (br-74o.10)
// ============================================================================

#[test]
fn test_write_results_creates_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let persister = ResultPersister::new(tmp.path());

    let results = vec![TestResult::new("test-installer").passed()];
    let path = persister.persist(&results, "run-1", chrono::Utc::now()).unwrap();

    assert!(path.exists());
    assert!(path.to_string_lossy().contains("results_"));
    assert!(path.to_string_lossy().ends_with(".jsonl"));
}

#[test]
fn test_write_results_valid_jsonl() {
    let tmp = tempfile::TempDir::new().unwrap();
    let persister = ResultPersister::new(tmp.path());

    let results = vec![
        TestResult::new("installer1").passed(),
        TestResult::new("installer2").failed(1, "error"),
    ];
    let path = persister.persist(&results, "run-2", chrono::Utc::now()).unwrap();

    let content = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();

    // 2 result entries + 1 summary = 3 lines
    assert_eq!(lines.len(), 3);

    // Each line should be valid JSON
    for line in &lines {
        assert!(serde_json::from_str::<serde_json::Value>(line).is_ok(), "Invalid JSON: {}", line);
    }

    // Last line should contain run_id (summary)
    assert!(lines[2].contains("run-2"));
}

#[test]
fn test_write_results_atomic_no_tmp() {
    let tmp = tempfile::TempDir::new().unwrap();
    let persister = ResultPersister::new(tmp.path());

    let results = vec![TestResult::new("test").passed()];
    let path = persister.persist(&results, "run-3", chrono::Utc::now()).unwrap();

    // After persist completes, no .tmp file should remain
    let tmp_files: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "tmp").unwrap_or(false))
        .collect();
    assert!(tmp_files.is_empty(), "No .tmp files should remain after persist");

    // The final file should exist
    assert!(path.exists());
}

#[test]
fn test_read_latest_results() {
    let tmp = tempfile::TempDir::new().unwrap();

    // Create two result files with different timestamps
    fs::write(tmp.path().join("results_20260101_120000.jsonl"), "{}\n").unwrap();
    fs::write(tmp.path().join("results_20260102_120000.jsonl"), "{}\n").unwrap();

    let persister = ResultPersister::new(tmp.path());
    let latest = persister.latest_results().unwrap();

    assert!(latest.is_some());
    let path = latest.unwrap();
    assert!(path.to_string_lossy().contains("20260102"), "Should return the newer file");
}

#[test]
fn test_read_results_empty_dir() {
    let tmp = tempfile::TempDir::new().unwrap();
    let persister = ResultPersister::new(tmp.path());

    let latest = persister.latest_results().unwrap();
    assert!(latest.is_none());
}

#[test]
fn test_read_results_nonexistent_dir() {
    let persister = ResultPersister::new("/nonexistent/path/that/doesnt/exist");
    let latest = persister.latest_results().unwrap();
    assert!(latest.is_none());
}

#[test]
fn test_result_entry_fields() {
    let tmp = tempfile::TempDir::new().unwrap();
    let persister = ResultPersister::new(tmp.path());

    let results = vec![TestResult::new("myinstaller").passed()];
    let path = persister.persist(&results, "run-fields", chrono::Utc::now()).unwrap();

    let (entries, summary) = ResultPersister::read_results(&path).unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].installer_name, "myinstaller");
    assert_eq!(entries[0].status, "passed");
    assert_eq!(entries[0].retry_count, 0);

    assert!(summary.is_some());
    let s = summary.unwrap();
    assert_eq!(s.run_id, "run-fields");
    assert_eq!(s.total, 1);
    assert_eq!(s.passed, 1);
    assert_eq!(s.failed, 0);
}

#[test]
fn test_results_dir_auto_created() {
    let tmp = tempfile::TempDir::new().unwrap();
    let nested = tmp.path().join("deeply").join("nested").join("dir");
    let persister = ResultPersister::new(&nested);

    let results = vec![TestResult::new("test").passed()];
    let path = persister.persist(&results, "run-nested", chrono::Utc::now()).unwrap();

    assert!(nested.exists());
    assert!(path.exists());
}
