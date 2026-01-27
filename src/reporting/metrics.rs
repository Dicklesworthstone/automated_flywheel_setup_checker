//! Prometheus metrics export

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

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
            self.success_rate_24h =
                self.successful_tests_24h as f64 / self.total_tests_24h as f64;
        }

        self.snapshot_time = Utc::now();
    }

    /// Record a remediation attempt
    pub fn record_remediation(&mut self) {
        self.total_remediations_24h += 1;
        self.snapshot_time = Utc::now();
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
        self.metrics.histograms.entry(key).or_insert_with(Vec::new).push(value);
    }

    /// Export metrics in Prometheus text format
    pub fn export(&self) -> String {
        let mut output = String::new();

        for (name, value) in &self.metrics.counters {
            output.push_str(&format!("{} {}\n", name, value));
        }

        for (name, value) in &self.metrics.gauges {
            output.push_str(&format!("{} {}\n", name, value));
        }

        output
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
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
}
