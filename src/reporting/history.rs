//! Run history: per-installer timelines, run-to-run diffs, flakiness and change-point detection.
//!
//! Everything here is derived from the persisted results files (`ResultPersister`), so it works
//! for `status --history`, `status --diff`, `check --failed-from`, longest-first scheduling,
//! change-only notifications, and the rolling metrics window without any extra state.
//!
//! Detectors (deterministic, unit-tested on synthetic series):
//! - Flakiness: Beta(1,1) posterior on the pass probability over outcomes since the last script
//!   hash change; `flaky` when the posterior mean is below 0.9 with at least 5 trials, at least
//!   two failures, and a pass observed after a failure (intermittent, not a trailing streak).
//! - Change point: CUSUM with fail = +1, pass = -0.5, threshold 3; `broken since <run>` when the
//!   statistic crosses and no pass follows the crossing.

use super::jsonl::{ResultEntry, ResultPersister, RunFile, RunHeader, RunInfo, RunSummaryEntry};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Statuses that count as a failure for reruns, diffs and notifications.
pub const FAILURE_STATUSES: &[&str] = &["failed", "timedout", "cancelled"];

pub fn is_failure_status(status: &str) -> bool {
    FAILURE_STATUSES.contains(&status)
}

/// One persisted run, fully loaded.
#[derive(Debug)]
pub struct LoadedRun {
    pub info: RunInfo,
    pub header: Option<RunHeader>,
    pub entries: Vec<ResultEntry>,
    pub summary: Option<RunSummaryEntry>,
}

impl LoadedRun {
    pub fn run_id(&self) -> &str {
        &self.info.run_id
    }

    pub fn started_at(&self) -> DateTime<Utc> {
        self.info.started_at
    }

    pub fn entry(&self, installer: &str) -> Option<&ResultEntry> {
        self.entries.iter().find(|e| e.installer_name == installer)
    }

    /// Installers that failed, timed out or were cancelled, with their category.
    pub fn failing_set(&self) -> BTreeMap<String, String> {
        self.entries
            .iter()
            .filter(|e| is_failure_status(&e.status))
            .map(|e| {
                let category = e
                    .error_classification
                    .as_ref()
                    .map(|c| c.category.clone())
                    .unwrap_or_else(|| "unknown".to_string());
                (e.installer_name.clone(), category)
            })
            .collect()
    }
}

/// One point on an installer's timeline.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HistoryEntry {
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub status: String,
    pub duration_ms: u64,
    pub attempts: usize,
    /// SHA-256 pinned for the script at the time (`checksum_expected`)
    pub script_sha256: Option<String>,
    pub installed_version: Option<String>,
    pub category: Option<String>,
}

/// Flakiness / breakage assessment for one installer.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Assessment {
    /// Outcomes considered (since the last script hash change)
    pub trials: usize,
    pub passes: usize,
    /// Beta(1,1) posterior mean of the pass probability
    pub pass_probability: f64,
    pub flaky: bool,
    /// Run id of the first failure of the current unbroken failure streak, when CUSUM crossed
    pub broken_since: Option<String>,
    /// Distinct script hashes seen across the whole timeline
    pub script_versions: usize,
}

impl Assessment {
    pub fn label(&self) -> Option<String> {
        if let Some(run) = &self.broken_since {
            return Some(format!("broken since {}", short(run)));
        }
        if self.flaky {
            return Some(format!("flaky ({:.0}% pass)", self.pass_probability * 100.0));
        }
        None
    }
}

fn short(run_id: &str) -> String {
    run_id.chars().take(8).collect()
}

/// Flakiness threshold on the posterior mean.
pub const FLAKY_THRESHOLD: f64 = 0.9;
/// Minimum trials before an installer can be called flaky.
pub const FLAKY_MIN_TRIALS: usize = 5;
/// Minimum failures (interleaved with passes) before an installer can be called flaky.
pub const FLAKY_MIN_FAILURES: usize = 2;
/// CUSUM parameters.
pub const CUSUM_FAIL: f64 = 1.0;
pub const CUSUM_PASS: f64 = -0.5;
pub const CUSUM_THRESHOLD: f64 = 3.0;

/// Assess a series of `(run_id, passed)` outcomes, oldest first.
pub fn assess_outcomes(series: &[(String, bool)]) -> Assessment {
    let trials = series.len();
    let passes = series.iter().filter(|(_, p)| *p).count();
    let failures = trials - passes;
    let pass_probability = (passes as f64 + 1.0) / (trials as f64 + 2.0);

    // CUSUM for a step change into persistent failure.
    let mut stat = 0.0_f64;
    let mut crossed_at: Option<usize> = None;
    for (idx, (_, passed)) in series.iter().enumerate() {
        stat = (stat + if *passed { CUSUM_PASS } else { CUSUM_FAIL }).max(0.0);
        if stat >= CUSUM_THRESHOLD && crossed_at.is_none() {
            crossed_at = Some(idx);
        }
        if *passed && crossed_at.is_some() {
            // A pass after the crossing: not broken (yet); restart detection.
            crossed_at = None;
            stat = 0.0;
        }
    }
    let broken_since = crossed_at.map(|idx| {
        // First failure of the trailing streak that produced the crossing.
        let mut start = idx;
        while start > 0 && !series[start - 1].1 {
            start -= 1;
        }
        series[start].0.clone()
    });

    // Intermittent means a pass came after a failure; a trailing failure streak is breakage
    // (CUSUM territory), and a single failure is noise, not flakiness.
    let first_failure = series.iter().position(|(_, p)| !*p);
    let interleaved = first_failure.is_some_and(|i| series[i..].iter().any(|(_, p)| *p));
    let flaky = broken_since.is_none()
        && trials >= FLAKY_MIN_TRIALS
        && failures >= FLAKY_MIN_FAILURES
        && interleaved
        && pass_probability < FLAKY_THRESHOLD;

    Assessment { trials, passes, pass_probability, flaky, broken_since, script_versions: 0 }
}

/// Change of one installer between two runs.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DiffEntry {
    pub installer: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub category_before: Option<String>,
    pub category_after: Option<String>,
    /// regressed | recovered | changed | added | removed
    pub change: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunDiff {
    pub from_run: String,
    pub to_run: String,
    pub from_started_at: DateTime<Utc>,
    pub to_started_at: DateTime<Utc>,
    pub changes: Vec<DiffEntry>,
    pub unchanged: usize,
}

impl RunDiff {
    pub fn regressions(&self) -> impl Iterator<Item = &DiffEntry> {
        self.changes.iter().filter(|c| c.change == "regressed")
    }
    pub fn recoveries(&self) -> impl Iterator<Item = &DiffEntry> {
        self.changes.iter().filter(|c| c.change == "recovered")
    }
}

fn category_of(entry: &ResultEntry) -> Option<String> {
    entry.error_classification.as_ref().map(|c| c.category.clone())
}

/// Compare two runs installer by installer.
pub fn diff_runs(from: &LoadedRun, to: &LoadedRun) -> RunDiff {
    let names: BTreeSet<&str> = from
        .entries
        .iter()
        .chain(to.entries.iter())
        .map(|e| e.installer_name.as_str())
        .collect();
    let mut changes = Vec::new();
    let mut unchanged = 0;
    for name in names {
        let a = from.entry(name);
        let b = to.entry(name);
        let before = a.map(|e| e.status.clone());
        let after = b.map(|e| e.status.clone());
        let category_before = a.and_then(category_of);
        let category_after = b.and_then(category_of);
        let change = match (a, b) {
            (None, Some(_)) => "added",
            (Some(_), None) => "removed",
            (Some(x), Some(y)) => {
                let was_failing = is_failure_status(&x.status);
                let is_failing = is_failure_status(&y.status);
                if x.status == y.status && category_before == category_after {
                    unchanged += 1;
                    continue;
                } else if !was_failing && is_failing {
                    "regressed"
                } else if was_failing && !is_failing {
                    "recovered"
                } else {
                    "changed"
                }
            }
            (None, None) => continue,
        };
        changes.push(DiffEntry {
            installer: name.to_string(),
            before,
            after,
            category_before,
            category_after,
            change: change.to_string(),
        });
    }
    RunDiff {
        from_run: from.run_id().to_string(),
        to_run: to.run_id().to_string(),
        from_started_at: from.started_at(),
        to_started_at: to.started_at(),
        changes,
        unchanged,
    }
}

/// All persisted runs, newest first.
#[derive(Debug, Default)]
pub struct History {
    runs: Vec<LoadedRun>,
}

impl History {
    /// Load every readable results file under `results_dir` (bounded by results retention).
    pub fn load(results_dir: &Path) -> Result<Self> {
        Self::load_recent(results_dir, usize::MAX)
    }

    /// Load at most `limit` newest runs.
    pub fn load_recent(results_dir: &Path, limit: usize) -> Result<Self> {
        if !results_dir.exists() {
            return Ok(Self::default());
        }
        let persister = ResultPersister::new(results_dir);
        let mut runs = Vec::new();
        for info in persister.list_runs()?.into_iter().take(limit) {
            let RunFile { header, entries, summary } = match ResultPersister::read_run_file(&info.path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            runs.push(LoadedRun { info, header, entries, summary });
        }
        Ok(Self { runs })
    }

    pub fn from_runs(mut runs: Vec<LoadedRun>) -> Self {
        runs.sort_by(|a, b| b.started_at().cmp(&a.started_at()));
        Self { runs }
    }

    /// Runs, newest first.
    pub fn runs(&self) -> &[LoadedRun] {
        &self.runs
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.runs.len()
    }

    pub fn latest(&self) -> Option<&LoadedRun> {
        self.runs.first()
    }

    /// Find by run id prefix or `last`.
    pub fn find(&self, prefix: &str) -> Option<&LoadedRun> {
        if prefix.eq_ignore_ascii_case("last") {
            return self.latest();
        }
        self.runs.iter().find(|r| r.run_id().starts_with(prefix))
    }

    /// The run that started before `run_id` (older neighbour).
    pub fn previous(&self, run_id: &str) -> Option<&LoadedRun> {
        let idx = self.runs.iter().position(|r| r.run_id() == run_id)?;
        self.runs.get(idx + 1)
    }

    /// Runs started within `window` of `now`, newest first.
    pub fn runs_within(&self, now: DateTime<Utc>, window: Duration) -> Vec<&LoadedRun> {
        let cutoff = now - window;
        self.runs.iter().filter(|r| r.started_at() >= cutoff && r.started_at() <= now).collect()
    }

    /// Every installer name seen in any run.
    pub fn installers(&self) -> BTreeSet<String> {
        self.runs.iter().flat_map(|r| r.entries.iter().map(|e| e.installer_name.clone())).collect()
    }

    /// Timeline for one installer, oldest first.
    pub fn installer_timeline(&self, installer: &str) -> Vec<HistoryEntry> {
        let mut out: Vec<HistoryEntry> = self
            .runs
            .iter()
            .rev()
            .filter_map(|run| {
                let e = run.entry(installer)?;
                Some(HistoryEntry {
                    run_id: run.run_id().to_string(),
                    started_at: run.started_at(),
                    status: e.status.clone(),
                    duration_ms: e.duration_ms,
                    attempts: e.attempts.len().max(1),
                    script_sha256: e.checksum_expected.clone(),
                    installed_version: e.installed_version.clone(),
                    category: category_of(e),
                })
            })
            .collect();
        out.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        out
    }

    /// Median duration over the installer's non-skipped outcomes.
    pub fn median_duration_ms(&self, installer: &str) -> Option<u64> {
        let mut d: Vec<u64> = self
            .installer_timeline(installer)
            .into_iter()
            .filter(|e| e.status != "skipped" && e.status != "cancelled")
            .map(|e| e.duration_ms)
            .collect();
        if d.is_empty() {
            return None;
        }
        d.sort_unstable();
        Some(d[d.len() / 2])
    }

    /// Flakiness / breakage assessment over outcomes since the last script hash change.
    pub fn assess(&self, installer: &str) -> Assessment {
        let timeline = self.installer_timeline(installer);
        let considered: Vec<&HistoryEntry> =
            timeline.iter().filter(|e| e.status != "skipped" && e.status != "cancelled").collect();
        let versions: BTreeSet<&str> =
            considered.iter().filter_map(|e| e.script_sha256.as_deref()).collect();
        // Segment: entries after the last change of the script hash.
        let mut start = 0;
        let mut last_hash: Option<&str> = None;
        for (idx, e) in considered.iter().enumerate() {
            if let Some(h) = e.script_sha256.as_deref() {
                if let Some(prev) = last_hash {
                    if prev != h {
                        start = idx;
                    }
                }
                last_hash = Some(h);
            }
        }
        let series: Vec<(String, bool)> =
            considered[start..].iter().map(|e| (e.run_id.clone(), e.status == "passed")).collect();
        let mut a = assess_outcomes(&series);
        a.script_versions = versions.len();
        a
    }

    /// Installers whose status in `run` was failed / timed out / cancelled.
    pub fn failed_installers(&self, run: &LoadedRun) -> Vec<String> {
        run.failing_set().into_keys().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::jsonl::ErrorClassificationEntry;

    fn entry(name: &str, status: &str, duration: u64, sha: &str) -> ResultEntry {
        ResultEntry {
            timestamp: Utc::now(),
            installer_name: name.into(),
            status: status.into(),
            duration_ms: duration,
            exit_code: Some(if status == "passed" { 0 } else { 1 }),
            error_classification: (status != "passed").then(|| ErrorClassificationEntry {
                category: "network".into(),
                severity: "transient".into(),
                retryable: true,
                confidence: 0.9,
            }),
            stderr_excerpt: String::new(),
            retry_count: 0,
            sha256_verified: true,
            checksum_state: "verified".into(),
            checksum_expected: Some(sha.into()),
            checksum_actual: None,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            container_id: None,
            last_attempt_ms: duration,
            attempts: Vec::new(),
            installed_version: None,
        }
    }

    fn run(id: &str, minutes_ago: i64, entries: Vec<ResultEntry>) -> LoadedRun {
        let started = Utc::now() - Duration::minutes(minutes_ago);
        LoadedRun {
            info: RunInfo {
                path: std::path::PathBuf::from(format!("/nowhere/{id}.jsonl")),
                run_id: id.into(),
                started_at: started,
                total: entries.len(),
                passed: entries.iter().filter(|e| e.status == "passed").count(),
                failed: entries.iter().filter(|e| is_failure_status(&e.status)).count(),
                interrupted: false,
            },
            header: None,
            entries,
            summary: None,
        }
    }

    fn series(pattern: &str) -> Vec<(String, bool)> {
        pattern.chars().enumerate().map(|(i, c)| (format!("run{i:02}"), c == 'P')).collect()
    }

    #[test]
    fn stable_series_is_neither_flaky_nor_broken() {
        let a = assess_outcomes(&series("PPPPPPPPPP"));
        assert!(!a.flaky);
        assert!(a.broken_since.is_none());
        assert!(a.pass_probability > 0.9);
        let none = assess_outcomes(&[]);
        assert_eq!(none.trials, 0);
        assert!(!none.flaky && none.broken_since.is_none());
    }

    #[test]
    fn intermittent_series_is_flaky_not_broken() {
        let a = assess_outcomes(&series("PFPFPFPF"));
        assert!(a.flaky, "{a:?}");
        assert!(a.broken_since.is_none(), "{a:?}");
        assert_eq!(a.trials, 8);
        assert_eq!(a.passes, 4);
        // Too few trials: never flaky.
        assert!(!assess_outcomes(&series("PFPF")).flaky);
        // A single early failure in a long passing series is not flaky.
        assert!(!assess_outcomes(&series("FPPPPPPPPPPPPPPP")).flaky);
        // Two trailing failures are not intermittent (not flaky, not yet broken).
        let trailing = assess_outcomes(&series("PPPPPFF"));
        assert!(!trailing.flaky && trailing.broken_since.is_none(), "{trailing:?}");
        // Two failures with a pass in between are.
        assert!(assess_outcomes(&series("PPPFPF")).flaky);
    }

    #[test]
    fn step_change_marks_broken_since_first_failure_of_the_streak() {
        let a = assess_outcomes(&series("PPPPPFFF"));
        assert_eq!(a.broken_since.as_deref(), Some("run05"), "{a:?}");
        assert!(!a.flaky);
        // A recovery after the crossing clears it.
        let recovered = assess_outcomes(&series("PPPPPFFFP"));
        assert!(recovered.broken_since.is_none(), "{recovered:?}");
        // Two failures are not enough evidence.
        assert!(assess_outcomes(&series("PPPPPFF")).broken_since.is_none());
    }

    #[test]
    fn assessment_only_considers_outcomes_since_the_last_script_change() {
        // Old script failed thrice, new script passes: not broken, not flaky.
        let runs = vec![
            run("r1", 60, vec![entry("tool", "failed", 10, "old")]),
            run("r2", 50, vec![entry("tool", "failed", 10, "old")]),
            run("r3", 40, vec![entry("tool", "failed", 10, "old")]),
            run("r4", 30, vec![entry("tool", "passed", 10, "new")]),
            run("r5", 20, vec![entry("tool", "passed", 10, "new")]),
        ];
        let h = History::from_runs(runs);
        let a = h.assess("tool");
        assert_eq!(a.trials, 2);
        assert!(a.broken_since.is_none() && !a.flaky, "{a:?}");
        assert_eq!(a.script_versions, 2);
        let timeline = h.installer_timeline("tool");
        assert_eq!(timeline.len(), 5);
        assert_eq!(timeline[0].run_id, "r1", "oldest first");
        assert_eq!(h.median_duration_ms("tool"), Some(10));
        assert_eq!(h.median_duration_ms("missing"), None);
    }

    #[test]
    fn diff_classifies_regressions_recoveries_additions_and_removals() {
        let a = run(
            "a",
            20,
            vec![entry("x", "passed", 1, "s"), entry("y", "failed", 1, "s"), entry("gone", "passed", 1, "s"), entry("same", "passed", 1, "s")],
        );
        let b = run(
            "b",
            10,
            vec![entry("x", "failed", 1, "s"), entry("y", "passed", 1, "s"), entry("new", "passed", 1, "s"), entry("same", "passed", 1, "s")],
        );
        let d = diff_runs(&a, &b);
        let by: BTreeMap<&str, &str> = d.changes.iter().map(|c| (c.installer.as_str(), c.change.as_str())).collect();
        assert_eq!(by["x"], "regressed");
        assert_eq!(by["y"], "recovered");
        assert_eq!(by["gone"], "removed");
        assert_eq!(by["new"], "added");
        assert!(!by.contains_key("same"));
        assert_eq!(d.unchanged, 1);
        assert_eq!(d.regressions().count(), 1);
        assert_eq!(d.recoveries().count(), 1);
        let h = History::from_runs(vec![a, b]);
        assert_eq!(h.latest().unwrap().run_id(), "b");
        assert_eq!(h.previous("b").unwrap().run_id(), "a");
        assert!(h.previous("a").is_none());
        assert_eq!(h.find("last").unwrap().run_id(), "b");
        assert_eq!(h.runs_within(Utc::now(), Duration::minutes(15)).len(), 1);
        assert_eq!(h.failed_installers(h.find("b").unwrap()), vec!["x".to_string()]);
    }

    #[test]
    fn load_reads_persisted_runs_and_tolerates_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(History::load(&dir.path().join("nope")).unwrap().is_empty());
        let persister = ResultPersister::new(dir.path());
        let results = vec![
            crate::runner::TestResult::new("tool").passed(),
            crate::runner::TestResult::new("other").failed(1, "boom"),
        ];
        let header = RunHeader::new("run-1");
        persister.persist_with_header(&results, &header, false).unwrap();
        let h = History::load(dir.path()).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h.installers().len(), 2);
        assert_eq!(h.latest().unwrap().failing_set().len(), 1);
    }
}
