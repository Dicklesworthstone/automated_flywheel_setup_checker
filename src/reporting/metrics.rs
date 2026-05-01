//! Prometheus metrics export

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Metrics data structure
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub counters: HashMap<String, u64>,
    pub gauges: HashMap<String, f64>,
    pub histograms: HashMap<String, Vec<f64>>,
}

/// Metrics snapshot saved to disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub last_test: Option<DateTime<Utc>>,
    pub last_success: Option<DateTime<Utc>>,
    pub last_failure: Option<DateTime<Utc>>,
    pub success_rate_24h: f64,
    pub total_tests_24h: u64,
    pub successful_tests_24h: u64,
    pub total_remediations_24h: u64,
    pub uptime_seconds: u64,
    pub snapshot_time: DateTime<Utc>,
}

impl Default for MetricsSnapshot {
    fn default() -> Self {
        Self {
            last_test: None,
            last_success: None,
            last_failure: None,
            success_rate_24h: 0.0,
            total_tests_24h: 0,
            successful_tests_24h: 0,
            total_remediations_24h: 0,
            uptime_seconds: 0,
            snapshot_time: Utc::now(),
        }
    }
}

impl MetricsSnapshot {
    /// Save metrics snapshot to a JSON file
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load metrics snapshot from a JSON file
    pub fn load(path: &Path) -> Result<Self> {
        let json = std::fs::read_to_string(path)?;
        let snapshot = serde_json::from_str(&json)?;
        Ok(snapshot)
    }

    /// Load a snapshot or fall back to the default empty state.
    pub fn load_or_default(path: &Path) -> Self {
        Self::load(path).unwrap_or_default()
    }

    /// Default metrics snapshot path (~/.local/share/afsc/metrics.json).
    pub fn default_path() -> PathBuf {
        default_data_dir().join("metrics.json")
    }

    /// Update snapshot with a new test result
    pub fn record_test(&mut self, success: bool) {
        self.last_test = Some(Utc::now());
        self.total_tests_24h += 1;

        if success {
            self.last_success = Some(Utc::now());
            self.successful_tests_24h += 1;
        } else {
            self.last_failure = Some(Utc::now());
        }

        // Recalculate success rate
        if self.total_tests_24h > 0 {
            self.success_rate_24h = self.successful_tests_24h as f64 / self.total_tests_24h as f64;
        }

        self.snapshot_time = Utc::now();
    }

    /// Record a remediation attempt
    pub fn record_remediation(&mut self) {
        self.total_remediations_24h += 1;
        self.snapshot_time = Utc::now();
    }

    /// Reset rolling counters when the snapshot is older than 24 hours.
    pub fn reset_if_stale(&mut self) {
        let age = Utc::now() - self.snapshot_time;

        if age > chrono::Duration::hours(24) {
            self.total_tests_24h = 0;
            self.successful_tests_24h = 0;
            self.success_rate_24h = 0.0;
            self.total_remediations_24h = 0;
        }
    }

    /// Update uptime
    pub fn set_uptime(&mut self, seconds: u64) {
        self.uptime_seconds = seconds;
        self.snapshot_time = Utc::now();
    }
}

/// Exports metrics in Prometheus format
pub struct MetricsExporter {
    metrics: Metrics,
    prefix: String,
}

impl MetricsExporter {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self { metrics: Metrics::default(), prefix: prefix.into() }
    }

    /// Build an exporter from a persisted metrics snapshot.
    pub fn from_snapshot(prefix: impl Into<String>, snapshot: &MetricsSnapshot) -> Self {
        let mut exporter = Self::new(prefix);
        exporter.set_gauge("tests_total_24h", snapshot.total_tests_24h as f64);
        exporter.set_gauge("successful_tests_24h", snapshot.successful_tests_24h as f64);
        exporter.set_gauge("success_rate_24h", snapshot.success_rate_24h);
        exporter.set_gauge("remediations_total_24h", snapshot.total_remediations_24h as f64);
        exporter.set_gauge("uptime_seconds", snapshot.uptime_seconds as f64);
        if let Some(last_test) = snapshot.last_test {
            exporter.set_gauge("last_test_timestamp", last_test.timestamp() as f64);
        }
        if let Some(last_success) = snapshot.last_success {
            exporter.set_gauge("last_success_timestamp", last_success.timestamp() as f64);
        }
        if let Some(last_failure) = snapshot.last_failure {
            exporter.set_gauge("last_failure_timestamp", last_failure.timestamp() as f64);
        }
        exporter
    }

    /// Increment a counter
    pub fn inc_counter(&mut self, name: &str) {
        let key = format!("{}_{}", self.prefix, name);
        *self.metrics.counters.entry(key).or_insert(0) += 1;
    }

    /// Set a gauge value
    pub fn set_gauge(&mut self, name: &str, value: f64) {
        let key = format!("{}_{}", self.prefix, name);
        self.metrics.gauges.insert(key, value);
    }

    /// Record a histogram value
    pub fn observe_histogram(&mut self, name: &str, value: f64) {
        let key = format!("{}_{}", self.prefix, name);
        self.metrics.histograms.entry(key).or_default().push(value);
    }

    /// Export metrics in Prometheus text format
    pub fn export(&self) -> String {
        let mut output = String::new();

        for (name, value) in &self.metrics.counters {
            output.push_str(&format!("# HELP {name} {}\n", metric_help(name)));
            output.push_str(&format!("# TYPE {name} counter\n"));
            output.push_str(&format!("{} {}\n", name, value));
        }

        for (name, value) in &self.metrics.gauges {
            output.push_str(&format!("# HELP {name} {}\n", metric_help(name)));
            output.push_str(&format!("# TYPE {name} gauge\n"));
            output.push_str(&format!("{} {}\n", name, value));
        }

        output
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }
}

fn default_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local").join("share").join("afsc")
}

fn metric_help(name: &str) -> &'static str {
    if name.ends_with("tests_total_24h") {
        "Total tests in last 24 hours"
    } else if name.ends_with("successful_tests_24h") {
        "Successful tests in last 24 hours"
    } else if name.ends_with("success_rate_24h") {
        "Success rate in last 24 hours"
    } else if name.ends_with("remediations_total_24h") {
        "Remediation attempts in last 24 hours"
    } else if name.ends_with("uptime_seconds") {
        "Most recent command runtime in seconds"
    } else if name.ends_with("last_test_timestamp") {
        "Unix timestamp of the most recent test"
    } else if name.ends_with("last_success_timestamp") {
        "Unix timestamp of the most recent successful test"
    } else if name.ends_with("last_failure_timestamp") {
        "Unix timestamp of the most recent failed test"
    } else {
        "AFSC metric"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_counter() {
        let mut exporter = MetricsExporter::new("test");
        exporter.inc_counter("requests");
        exporter.inc_counter("requests");

        assert_eq!(exporter.metrics.counters.get("test_requests"), Some(&2));
    }

    #[test]
    fn test_gauge() {
        let mut exporter = MetricsExporter::new("test");
        exporter.set_gauge("temperature", 23.5);

        assert_eq!(exporter.metrics.gauges.get("test_temperature"), Some(&23.5));
    }

    #[test]
    fn test_metrics_snapshot_save_load() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("metrics.json");

        let mut snapshot = MetricsSnapshot::default();
        snapshot.record_test(true);
        snapshot.record_test(true);
        snapshot.record_test(false);
        snapshot.record_remediation();
        snapshot.set_uptime(3600);

        snapshot.save(&path).unwrap();
        let loaded = MetricsSnapshot::load(&path).unwrap();

        assert_eq!(loaded.total_tests_24h, 3);
        assert_eq!(loaded.total_remediations_24h, 1);
        assert_eq!(loaded.uptime_seconds, 3600);
        assert!(loaded.last_test.is_some());
    }

    #[test]
    fn test_metrics_snapshot_success_rate() {
        let mut snapshot = MetricsSnapshot::default();

        // All successes
        snapshot.record_test(true);
        snapshot.record_test(true);
        assert!((snapshot.success_rate_24h - 1.0).abs() < 0.01);

        // One failure
        snapshot.record_test(false);
        // Now 2 successes out of 3 = 0.666...
        assert!((snapshot.success_rate_24h - 0.666).abs() < 0.01);
    }

    #[test]
    fn test_metrics_snapshot_reset_if_stale() {
        let mut snapshot = MetricsSnapshot {
            total_tests_24h: 10,
            successful_tests_24h: 8,
            success_rate_24h: 0.8,
            total_remediations_24h: 2,
            snapshot_time: Utc::now() - chrono::Duration::hours(25),
            ..Default::default()
        };

        snapshot.reset_if_stale();

        assert_eq!(snapshot.total_tests_24h, 0);
        assert_eq!(snapshot.successful_tests_24h, 0);
        assert_eq!(snapshot.success_rate_24h, 0.0);
        assert_eq!(snapshot.total_remediations_24h, 0);
    }

    #[test]
    fn test_metrics_snapshot_no_reset_if_fresh() {
        let mut snapshot = MetricsSnapshot {
            total_tests_24h: 10,
            successful_tests_24h: 8,
            success_rate_24h: 0.8,
            total_remediations_24h: 2,
            snapshot_time: Utc::now() - chrono::Duration::hours(23),
            ..Default::default()
        };

        snapshot.reset_if_stale();

        assert_eq!(snapshot.total_tests_24h, 10);
        assert_eq!(snapshot.successful_tests_24h, 8);
        assert_eq!(snapshot.success_rate_24h, 0.8);
        assert_eq!(snapshot.total_remediations_24h, 2);
    }

    #[test]
    fn test_metrics_file_path_convention() {
        let expected = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
            .join(".local")
            .join("share")
            .join("afsc")
            .join("metrics.json");

        assert_eq!(MetricsSnapshot::default_path(), expected);
    }

    #[test]
    fn test_metrics_exporter_from_snapshot_prometheus_text() {
        let snapshot = MetricsSnapshot {
            total_tests_24h: 42,
            successful_tests_24h: 40,
            success_rate_24h: 0.952,
            total_remediations_24h: 3,
            ..Default::default()
        };

        let exporter = MetricsExporter::from_snapshot("afsc", &snapshot);
        let output = exporter.export();

        assert!(output.contains("# HELP afsc_tests_total_24h Total tests in last 24 hours"));
        assert!(output.contains("# TYPE afsc_tests_total_24h gauge"));
        assert!(output.contains("afsc_tests_total_24h 42"));
        assert!(output.contains("# HELP afsc_success_rate_24h Success rate in last 24 hours"));
        assert!(output.contains("afsc_success_rate_24h 0.952"));
    }
}
