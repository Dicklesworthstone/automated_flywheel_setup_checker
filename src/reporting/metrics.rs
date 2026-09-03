//! Metrics and health derived from persisted runs.
//!
//! Nothing here is a counter kept in memory: every value is recomputed from the results files
//! (rolling 24 h window by run start time) plus the last `validate --check-hashes` report, so
//! `status --format prometheus`, `/metrics` and `/health` are deterministic functions of the data
//! directory. `MetricsSnapshot` (`metrics.json`) is kept as a compact, human-readable cache of the
//! same numbers.

use super::history::{is_failure_status, History};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Rolling window for the `_24h` series.
pub const WINDOW: Duration = Duration::hours(24);

/// Metrics snapshot saved to disk (`<data_dir>/metrics.json`).
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
    /// Runs that started within the window
    #[serde(default)]
    pub runs_24h: u64,
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
            runs_24h: 0,
        }
    }
}

impl MetricsSnapshot {
    /// Save metrics snapshot to a JSON file (atomically: temp file + rename)
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        crate::lock::write_atomic(path, json.as_bytes())?;
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

        if age > WINDOW {
            self.total_tests_24h = 0;
            self.successful_tests_24h = 0;
            self.success_rate_24h = 0.0;
            self.total_remediations_24h = 0;
            self.runs_24h = 0;
        }
    }

    /// Update uptime
    pub fn set_uptime(&mut self, seconds: u64) {
        self.uptime_seconds = seconds;
        self.snapshot_time = Utc::now();
    }
}

/// Per-installer series from the most recent run that included the installer.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InstallerMetric {
    /// 1 passed, 0 failed/timed out, -1 skipped/cancelled
    pub status: i64,
    pub duration_seconds: f64,
    pub attempts: u64,
    pub run_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    NoData,
    Ok,
    Stale,
}

impl HealthState {
    pub fn as_str(self) -> &'static str {
        match self {
            HealthState::NoData => "no_data",
            HealthState::Ok => "ok",
            HealthState::Stale => "stale",
        }
    }
}

/// Persisted by `validate --check-hashes` (`<data_dir>/validate.json`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ValidationReport {
    pub checked_at: Option<DateTime<Utc>>,
    pub checksums_path: String,
    pub total: u64,
    pub matched: u64,
    pub mismatched: Vec<String>,
    pub unreachable: Vec<String>,
}

impl ValidationReport {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("validate.json")
    }

    pub fn load(data_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(data_dir)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let json = serde_json::to_string_pretty(self)?;
        crate::lock::write_atomic(&Self::path(data_dir), json.as_bytes())?;
        Ok(())
    }

    pub fn drift_total(&self) -> u64 {
        (self.mismatched.len() + self.unreachable.len()) as u64
    }
}

/// Everything `/metrics`, `/health` and `status --format prometheus` report.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsReport {
    pub computed_at: DateTime<Utc>,
    pub health: HealthState,
    pub last_run_id: Option<String>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_run_age_seconds: Option<i64>,
    pub last_run_interrupted: bool,
    pub last_run_duration_seconds: f64,
    pub last_success: Option<DateTime<Utc>>,
    pub last_failure: Option<DateTime<Utc>>,
    pub runs_24h: u64,
    pub total_tests_24h: u64,
    pub successful_tests_24h: u64,
    pub failed_tests_24h: u64,
    pub success_rate_24h: f64,
    pub total_remediations_24h: u64,
    pub remediations_verified_24h: u64,
    pub remediations_cost_usd_24h: f64,
    pub checksum_drift_total: Option<u64>,
    pub validated_at: Option<DateTime<Utc>>,
    pub installers: BTreeMap<String, InstallerMetric>,
    pub stale_after_seconds: u64,
}

impl MetricsReport {
    /// Compute from a loaded history at `now`.
    pub fn compute(
        history: &History,
        now: DateTime<Utc>,
        stale_after_seconds: u64,
        validation: Option<&ValidationReport>,
        remediations_24h: u64,
    ) -> Self {
        let window: Vec<_> = history.runs_within(now, WINDOW);
        let mut total = 0u64;
        let mut successful = 0u64;
        let mut failed = 0u64;
        let mut last_success: Option<DateTime<Utc>> = None;
        let mut last_failure: Option<DateTime<Utc>> = None;
        for run in history.runs() {
            for e in &run.entries {
                if e.status == "passed" {
                    last_success = last_success.max(Some(e.timestamp));
                } else if is_failure_status(&e.status) {
                    last_failure = last_failure.max(Some(e.timestamp));
                }
            }
        }
        let mut remediations = 0u64;
        let mut remediations_verified = 0u64;
        let mut remediations_cost = 0.0f64;
        for run in &window {
            for e in &run.entries {
                if let Some(r) = &e.remediation {
                    if r.attempted() {
                        remediations += 1;
                        remediations_cost += r.cost_usd();
                    }
                    if r.succeeded() {
                        remediations_verified += 1;
                    }
                }
                if e.status == "skipped" {
                    continue;
                }
                total += 1;
                if e.status == "passed" {
                    successful += 1;
                } else if is_failure_status(&e.status) {
                    failed += 1;
                }
            }
        }

        // Per-installer: the newest run that includes each installer.
        let mut installers = BTreeMap::new();
        for run in history.runs() {
            for e in &run.entries {
                installers.entry(e.installer_name.clone()).or_insert_with(|| InstallerMetric {
                    status: match e.status.as_str() {
                        "passed" => 1,
                        "failed" | "timedout" => 0,
                        _ => -1,
                    },
                    duration_seconds: e.duration_ms as f64 / 1000.0,
                    attempts: e.attempts.len().max(1) as u64,
                    run_id: run.run_id().to_string(),
                });
            }
        }

        let latest = history.latest();
        let last_run_at = latest.map(|r| r.started_at());
        let age = last_run_at.map(|t| (now - t).num_seconds());
        let health = match age {
            None => HealthState::NoData,
            Some(a) if a > stale_after_seconds as i64 => HealthState::Stale,
            Some(_) => HealthState::Ok,
        };

        Self {
            computed_at: now,
            health,
            last_run_id: latest.map(|r| r.run_id().to_string()),
            last_run_at,
            last_run_age_seconds: age,
            last_run_interrupted: latest.map(|r| r.info.interrupted).unwrap_or(false),
            last_run_duration_seconds: latest
                .and_then(|r| r.summary.as_ref())
                .map(|s| s.duration_total_ms as f64 / 1000.0)
                .unwrap_or(0.0),
            last_success,
            last_failure,
            runs_24h: window.len() as u64,
            total_tests_24h: total,
            successful_tests_24h: successful,
            failed_tests_24h: failed,
            success_rate_24h: if total > 0 { successful as f64 / total as f64 } else { 0.0 },
            // Outcomes persisted on results are authoritative; the legacy snapshot counter covers
            // runs recorded before outcomes existed.
            total_remediations_24h: remediations.max(remediations_24h),
            remediations_verified_24h: remediations_verified,
            remediations_cost_usd_24h: remediations_cost,
            checksum_drift_total: validation.map(|v| v.drift_total()),
            validated_at: validation.and_then(|v| v.checked_at),
            installers,
            stale_after_seconds,
        }
    }

    /// Load history and the validation report from a data dir and compute.
    pub fn from_data_dir(
        data_dir: &Path,
        now: DateTime<Utc>,
        stale_after_seconds: u64,
    ) -> Result<Self> {
        let history = History::load(&data_dir.join("results"))?;
        let validation = ValidationReport::load(data_dir);
        let remediations = MetricsSnapshot::load(&data_dir.join("metrics.json"))
            .ok()
            .filter(|s| now - s.snapshot_time <= WINDOW)
            .map(|s| s.total_remediations_24h)
            .unwrap_or(0);
        Ok(Self::compute(&history, now, stale_after_seconds, validation.as_ref(), remediations))
    }

    /// The compact snapshot for `metrics.json`.
    pub fn snapshot(&self, remediations_24h: u64, uptime_seconds: u64) -> MetricsSnapshot {
        MetricsSnapshot {
            last_test: self.last_run_at,
            last_success: self.last_success,
            last_failure: self.last_failure,
            success_rate_24h: self.success_rate_24h,
            total_tests_24h: self.total_tests_24h,
            successful_tests_24h: self.successful_tests_24h,
            total_remediations_24h: remediations_24h,
            uptime_seconds,
            snapshot_time: self.computed_at,
            runs_24h: self.runs_24h,
        }
    }

    /// Health document for `/health`.
    pub fn health_json(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.health.as_str(),
            "last_run_id": self.last_run_id,
            "last_run": self.last_run_at,
            "last_run_age_seconds": self.last_run_age_seconds,
            "last_run_interrupted": self.last_run_interrupted,
            "stale_after_seconds": self.stale_after_seconds,
            "last_success": self.last_success,
            "last_failure": self.last_failure,
            "runs_24h": self.runs_24h,
            "total_tests_24h": self.total_tests_24h,
            "successful_tests_24h": self.successful_tests_24h,
            "failed_tests_24h": self.failed_tests_24h,
            "success_rate_24h": self.success_rate_24h,
            "total_remediations_24h": self.total_remediations_24h,
            "remediations_verified_24h": self.remediations_verified_24h,
            "remediations_cost_usd_24h": self.remediations_cost_usd_24h,
            "checksum_drift_total": self.checksum_drift_total,
            "validated_at": self.validated_at,
            "computed_at": self.computed_at,
        })
    }

    /// Prometheus text exposition (families sorted, labels sorted: byte-identical for equal input).
    pub fn to_prometheus(&self) -> String {
        let mut x = MetricsExporter::new("afsc");
        x.set_gauge("tests_total_24h", self.total_tests_24h as f64);
        x.set_gauge("successful_tests_24h", self.successful_tests_24h as f64);
        x.set_gauge("failed_tests_24h", self.failed_tests_24h as f64);
        x.set_gauge("success_rate_24h", self.success_rate_24h);
        x.set_gauge("runs_24h", self.runs_24h as f64);
        x.set_gauge("remediations_total_24h", self.total_remediations_24h as f64);
        x.set_gauge("remediations_verified_24h", self.remediations_verified_24h as f64);
        x.set_gauge("remediations_cost_usd_24h", self.remediations_cost_usd_24h);
        x.set_gauge(
            "health",
            match self.health {
                HealthState::Ok => 1.0,
                HealthState::Stale => 0.0,
                HealthState::NoData => -1.0,
            },
        );
        if let Some(t) = self.last_run_at {
            x.set_gauge("run_last_timestamp", t.timestamp() as f64);
            x.set_gauge("last_test_timestamp", t.timestamp() as f64);
        }
        if let Some(age) = self.last_run_age_seconds {
            x.set_gauge("run_last_age_seconds", age as f64);
        }
        x.set_gauge("run_last_interrupted", if self.last_run_interrupted { 1.0 } else { 0.0 });
        x.set_gauge("run_last_duration_seconds", self.last_run_duration_seconds);
        if let Some(t) = self.last_success {
            x.set_gauge("last_success_timestamp", t.timestamp() as f64);
        }
        if let Some(t) = self.last_failure {
            x.set_gauge("last_failure_timestamp", t.timestamp() as f64);
        }
        if let Some(d) = self.checksum_drift_total {
            x.set_gauge("checksum_drift_total", d as f64);
        }
        if let Some(t) = self.validated_at {
            x.set_gauge("validate_last_timestamp", t.timestamp() as f64);
        }
        for (name, m) in &self.installers {
            let labels = [("installer", name.as_str())];
            x.set_labeled_gauge("installer_status", &labels, m.status as f64);
            x.set_labeled_gauge("installer_duration_seconds", &labels, m.duration_seconds);
            x.set_labeled_gauge("installer_attempts", &labels, m.attempts as f64);
        }
        x.export()
    }
}

/// One metric family: help text, type and its samples keyed by rendered label set.
#[derive(Debug, Clone, Default)]
struct Family {
    kind: &'static str,
    samples: BTreeMap<String, f64>,
}

/// Exports metrics in Prometheus text format with deterministic ordering.
#[derive(Debug, Clone)]
pub struct MetricsExporter {
    families: BTreeMap<String, Family>,
    prefix: String,
}

impl MetricsExporter {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self { families: BTreeMap::new(), prefix: prefix.into() }
    }

    /// Build an exporter from a persisted metrics snapshot.
    pub fn from_snapshot(prefix: impl Into<String>, snapshot: &MetricsSnapshot) -> Self {
        let mut exporter = Self::new(prefix);
        exporter.set_gauge("tests_total_24h", snapshot.total_tests_24h as f64);
        exporter.set_gauge("successful_tests_24h", snapshot.successful_tests_24h as f64);
        exporter.set_gauge("success_rate_24h", snapshot.success_rate_24h);
        exporter.set_gauge("remediations_total_24h", snapshot.total_remediations_24h as f64);
        exporter.set_gauge("uptime_seconds", snapshot.uptime_seconds as f64);
        exporter.set_gauge("runs_24h", snapshot.runs_24h as f64);
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

    fn family(&mut self, name: &str, kind: &'static str) -> &mut Family {
        let key = format!("{}_{}", self.prefix, name);
        let f = self.families.entry(key).or_default();
        f.kind = kind;
        f
    }

    /// Increment a counter
    pub fn inc_counter(&mut self, name: &str) {
        *self.family(name, "counter").samples.entry(String::new()).or_insert(0.0) += 1.0;
    }

    /// Set a gauge value
    pub fn set_gauge(&mut self, name: &str, value: f64) {
        self.family(name, "gauge").samples.insert(String::new(), value);
    }

    /// Set a labeled gauge sample, e.g. `installer_status{installer="rust"}`.
    pub fn set_labeled_gauge(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        let mut sorted: Vec<(&str, &str)> = labels.to_vec();
        sorted.sort();
        let rendered = sorted
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
            .collect::<Vec<_>>()
            .join(",");
        self.family(name, "gauge").samples.insert(format!("{{{rendered}}}"), value);
    }

    /// Current value of an unlabeled sample.
    pub fn value(&self, name: &str) -> Option<f64> {
        self.families.get(&format!("{}_{}", self.prefix, name))?.samples.get("").copied()
    }

    /// Export metrics in Prometheus text format
    pub fn export(&self) -> String {
        let mut output = String::new();
        for (name, family) in &self.families {
            output.push_str(&format!("# HELP {name} {}\n", metric_help(name)));
            output.push_str(&format!("# TYPE {name} {}\n", family.kind));
            for (labels, value) in &family.samples {
                output.push_str(&format!("{name}{labels} {}\n", format_value(*value)));
            }
        }
        output
    }
}

fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn format_value(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn default_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local").join("share").join("afsc")
}

fn metric_help(name: &str) -> &'static str {
    if name.ends_with("tests_total_24h") {
        "Installer tests in the last 24 hours (skips excluded)"
    } else if name.ends_with("successful_tests_24h") {
        "Passed installer tests in the last 24 hours"
    } else if name.ends_with("failed_tests_24h") {
        "Failed, timed out or cancelled installer tests in the last 24 hours"
    } else if name.ends_with("success_rate_24h") {
        "Pass ratio in the last 24 hours"
    } else if name.ends_with("runs_24h") {
        "Runs started in the last 24 hours"
    } else if name.ends_with("remediations_total_24h") {
        "Remediation attempts in the last 24 hours"
    } else if name.ends_with("remediations_verified_24h") {
        "Remediations verified or applied in the last 24 hours"
    } else if name.ends_with("remediations_cost_usd_24h") {
        "Claude spend attributed to remediation in the last 24 hours"
    } else if name.ends_with("uptime_seconds") {
        "Most recent command runtime in seconds"
    } else if name.ends_with("health") {
        "1 ok, 0 stale (last run older than monitoring.stale_after_seconds), -1 no data"
    } else if name.ends_with("run_last_timestamp") || name.ends_with("last_test_timestamp") {
        "Unix timestamp of the most recent run"
    } else if name.ends_with("run_last_age_seconds") {
        "Seconds since the most recent run started"
    } else if name.ends_with("run_last_interrupted") {
        "1 when the most recent run was interrupted"
    } else if name.ends_with("run_last_duration_seconds") {
        "Wall-clock duration of the most recent run"
    } else if name.ends_with("last_success_timestamp") {
        "Unix timestamp of the most recent passed installer test"
    } else if name.ends_with("last_failure_timestamp") {
        "Unix timestamp of the most recent failed installer test"
    } else if name.ends_with("checksum_drift_total") {
        "Mismatched checksums plus unreachable URLs in the last validate --check-hashes"
    } else if name.ends_with("validate_last_timestamp") {
        "Unix timestamp of the last validate --check-hashes"
    } else if name.ends_with("installer_status") {
        "Latest outcome per installer: 1 passed, 0 failed/timed out, -1 skipped/cancelled"
    } else if name.ends_with("installer_duration_seconds") {
        "Latest total duration per installer (all attempts)"
    } else if name.ends_with("installer_attempts") {
        "Attempts used in the latest run per installer"
    } else {
        "AFSC metric"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::history::LoadedRun;
    use crate::reporting::jsonl::{ResultEntry, RunInfo, RunSummaryEntry};
    use tempfile::TempDir;

    fn entry(name: &str, status: &str, at: DateTime<Utc>) -> ResultEntry {
        ResultEntry {
            timestamp: at,
            installer_name: name.into(),
            status: status.into(),
            duration_ms: 1500,
            exit_code: Some(0),
            error_classification: None,
            stderr_excerpt: String::new(),
            retry_count: 0,
            sha256_verified: true,
            checksum_state: "verified".into(),
            checksum_expected: None,
            checksum_actual: None,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            container_id: None,
            last_attempt_ms: 1500,
            attempts: Vec::new(),
            installed_version: None,
            remediation: None,
            telemetry: None,
        }
    }

    fn run(id: &str, hours_ago: i64, statuses: &[(&str, &str)], now: DateTime<Utc>) -> LoadedRun {
        let at = now - Duration::hours(hours_ago);
        let entries: Vec<ResultEntry> = statuses.iter().map(|(n, s)| entry(n, s, at)).collect();
        LoadedRun {
            info: RunInfo {
                path: PathBuf::from(format!("/nowhere/{id}.jsonl")),
                run_id: id.into(),
                started_at: at,
                total: entries.len(),
                passed: entries.iter().filter(|e| e.status == "passed").count(),
                failed: entries.iter().filter(|e| is_failure_status(&e.status)).count(),
                interrupted: false,
            },
            header: None,
            summary: Some(RunSummaryEntry {
                run_id: id.into(),
                total: entries.len(),
                passed: 0,
                failed: 0,
                skipped: 0,
                timed_out: 0,
                cancelled: 0,
                duration_total_ms: 4200,
                timestamp_start: at,
                timestamp_end: at,
                interrupted: false,
                exit_code: 0,
            }),
            entries,
        }
    }

    #[test]
    fn window_counts_only_runs_within_24_hours() {
        let now = Utc::now();
        let history = History::from_runs(vec![
            run("r1h", 1, &[("a", "passed"), ("b", "failed"), ("c", "skipped")], now),
            run("r23h", 23, &[("a", "passed"), ("b", "passed")], now),
            run("r25h", 25, &[("a", "failed"), ("b", "failed"), ("d", "passed")], now),
        ]);
        let m = MetricsReport::compute(&history, now, 93_600, None, 0);
        assert_eq!(m.runs_24h, 2);
        assert_eq!(m.total_tests_24h, 4, "skips excluded, 25 h run excluded");
        assert_eq!(m.successful_tests_24h, 3);
        assert_eq!(m.failed_tests_24h, 1);
        assert!((m.success_rate_24h - 0.75).abs() < 1e-9);
        assert_eq!(m.health, HealthState::Ok);
        assert_eq!(m.last_run_id.as_deref(), Some("r1h"));
        // Per-installer series come from the newest run containing the installer.
        assert_eq!(m.installers["a"].status, 1);
        assert_eq!(m.installers["b"].status, 0);
        assert_eq!(m.installers["c"].status, -1);
        assert_eq!(m.installers["d"].run_id, "r25h");
        assert!(m.last_success.is_some() && m.last_failure.is_some());
    }

    #[test]
    fn health_transitions_no_data_ok_stale() {
        let now = Utc::now();
        let empty = MetricsReport::compute(&History::default(), now, 100, None, 0);
        assert_eq!(empty.health, HealthState::NoData);
        assert!(empty.to_prometheus().contains("afsc_health -1\n"));
        let fresh = History::from_runs(vec![run("r", 0, &[("a", "passed")], now)]);
        assert_eq!(MetricsReport::compute(&fresh, now, 100, None, 0).health, HealthState::Ok);
        let old = History::from_runs(vec![run("r", 30, &[("a", "passed")], now)]);
        let m = MetricsReport::compute(&old, now, 93_600, None, 0);
        assert_eq!(m.health, HealthState::Stale);
        assert_eq!(m.health_json()["status"], "stale");
        assert_eq!(m.runs_24h, 0, "stale run is outside the window");
    }

    #[test]
    fn prometheus_output_is_sorted_labeled_and_stable() {
        let now = Utc::now();
        let history =
            History::from_runs(vec![run("r", 1, &[("zeta", "passed"), ("alpha", "failed")], now)]);
        let v = ValidationReport {
            mismatched: vec!["x".into(), "y".into()],
            unreachable: vec!["z".into()],
            ..Default::default()
        };
        let m = MetricsReport::compute(&history, now, 93_600, Some(&v), 2);
        let text = m.to_prometheus();
        assert_eq!(text, m.to_prometheus(), "deterministic");
        assert!(text.contains("afsc_installer_status{installer=\"alpha\"} 0\n"), "{text}");
        assert!(text.contains("afsc_installer_status{installer=\"zeta\"} 1\n"), "{text}");
        assert!(
            text.contains("afsc_installer_duration_seconds{installer=\"alpha\"} 1.5\n"),
            "{text}"
        );
        assert!(text.contains("afsc_checksum_drift_total 3\n"), "{text}");
        assert!(text.contains("afsc_remediations_total_24h 2\n"), "{text}");
        assert!(text.contains("afsc_run_last_duration_seconds 4.2\n"), "{text}");
        let alpha = text.find("installer=\"alpha\"").unwrap();
        let zeta = text.find("installer=\"zeta\"").unwrap();
        assert!(alpha < zeta, "labels sorted");
        let families: Vec<&str> = text.lines().filter(|l| l.starts_with("# TYPE")).collect();
        let mut sorted = families.clone();
        sorted.sort();
        assert_eq!(families, sorted, "families sorted");
        assert_eq!(text.matches("# TYPE afsc_installer_status").count(), 1, "one family header");
    }

    #[test]
    fn exporter_counters_gauges_and_label_escaping() {
        let mut exporter = MetricsExporter::new("test");
        exporter.inc_counter("requests");
        exporter.inc_counter("requests");
        assert_eq!(exporter.value("requests"), Some(2.0));
        exporter.set_gauge("temperature", 23.5);
        assert_eq!(exporter.value("temperature"), Some(23.5));
        exporter.set_labeled_gauge("thing", &[("b", "2"), ("a", "he said \"hi\"")], 1.0);
        let text = exporter.export();
        assert!(text.contains("test_thing{a=\"he said \\\"hi\\\"\",b=\"2\"} 1\n"), "{text}");
        assert!(text.contains("# TYPE test_requests counter\n"));
    }

    #[test]
    fn snapshot_save_load_and_stale_reset() {
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
        assert!((loaded.success_rate_24h - 0.666).abs() < 0.01);

        let mut stale = MetricsSnapshot {
            total_tests_24h: 10,
            successful_tests_24h: 8,
            success_rate_24h: 0.8,
            total_remediations_24h: 2,
            snapshot_time: Utc::now() - Duration::hours(25),
            ..Default::default()
        };
        stale.reset_if_stale();
        assert_eq!(stale.total_tests_24h, 0);
        let mut fresh = MetricsSnapshot {
            total_tests_24h: 10,
            snapshot_time: Utc::now() - Duration::hours(23),
            ..Default::default()
        };
        fresh.reset_if_stale();
        assert_eq!(fresh.total_tests_24h, 10);
        assert_eq!(MetricsSnapshot::default_path().file_name().unwrap(), "metrics.json");

        let text = MetricsExporter::from_snapshot("afsc", &loaded).export();
        assert!(text.contains("# HELP afsc_tests_total_24h"), "{text}");
        assert!(text.contains("afsc_tests_total_24h 3\n"), "{text}");
    }

    #[test]
    fn validation_report_round_trips_and_counts_drift() {
        let tmp = TempDir::new().unwrap();
        let report = ValidationReport {
            checked_at: Some(Utc::now()),
            checksums_path: "/x/checksums.yaml".into(),
            total: 5,
            matched: 3,
            mismatched: vec!["a".into()],
            unreachable: vec!["b".into()],
        };
        report.save(tmp.path()).unwrap();
        let loaded = ValidationReport::load(tmp.path()).unwrap();
        assert_eq!(loaded.drift_total(), 2);
        assert!(ValidationReport::load(&tmp.path().join("nope")).is_none());
    }
}
