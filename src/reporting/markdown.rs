//! Markdown rendering of runs, timelines and diffs (shared by `status --format markdown`,
//! GitHub issue bodies and the nightly canary).

use super::history::{Assessment, HistoryEntry, LoadedRun, RunDiff};
use std::collections::BTreeMap;

fn short(run_id: &str) -> String {
    run_id.chars().take(8).collect()
}

fn icon(status: &str) -> &'static str {
    match status {
        "passed" => "✅",
        "failed" => "❌",
        "timedout" => "⏱️",
        "cancelled" => "⊘",
        "skipped" => "➖",
        _ => "❔",
    }
}

fn seconds(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// One run as a Markdown section with a per-installer table.
pub fn render_run(run: &LoadedRun, assessments: &BTreeMap<String, Assessment>) -> String {
    let mut out = String::new();
    let (passed, failed, skipped, interrupted) = match &run.summary {
        Some(s) => (s.passed, s.failed, s.skipped, s.interrupted),
        None => (
            run.entries.iter().filter(|e| e.status == "passed").count(),
            run.entries.iter().filter(|e| super::history::is_failure_status(&e.status)).count(),
            run.entries.iter().filter(|e| e.status == "skipped").count(),
            false,
        ),
    };
    out.push_str(&format!(
        "## AFSC run `{}` — {} passed, {} failed, {} skipped ({} UTC){}\n\n",
        short(run.run_id()),
        passed,
        failed,
        skipped,
        run.started_at().format("%Y-%m-%d %H:%M"),
        if interrupted { " — **interrupted**" } else { "" }
    ));
    out.push_str("| installer | status | category | duration | attempts | note |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    let mut entries: Vec<_> = run.entries.iter().collect();
    entries.sort_by(|a, b| {
        let fa = super::history::is_failure_status(&a.status);
        let fb = super::history::is_failure_status(&b.status);
        fb.cmp(&fa).then_with(|| a.installer_name.cmp(&b.installer_name))
    });
    for e in entries {
        let category = e.error_classification.as_ref().map(|c| c.category.as_str()).unwrap_or("");
        let mut note = assessments.get(&e.installer_name).and_then(|a| a.label()).unwrap_or_default();
        if e.checksum_state == "mismatch" {
            note = if note.is_empty() { "checksum mismatch".into() } else { format!("{note}; checksum mismatch") };
        }
        if let Some(v) = &e.installed_version {
            note = if note.is_empty() { v.clone() } else { format!("{note}; {v}") };
        }
        out.push_str(&format!(
            "| {} | {} {} | {} | {} | {} | {} |\n",
            e.installer_name,
            icon(&e.status),
            e.status,
            category,
            seconds(e.duration_ms),
            e.attempts.len().max(1),
            note
        ));
    }
    if let Some(h) = &run.header {
        out.push_str(&format!(
            "\nBackend: {}{}, parallel {}, timeout {}s, retries {}, tool v{}.\n",
            h.backend,
            h.image.as_ref().map(|i| format!(" ({i})")).unwrap_or_default(),
            h.parallel,
            h.timeout_seconds,
            h.retries,
            h.tool_version
        ));
    }
    out
}

/// Timeline of one installer, oldest first.
pub fn render_timeline(installer: &str, entries: &[HistoryEntry], assessment: &Assessment) -> String {
    let mut out = format!("## {installer} — {} runs", entries.len());
    if let Some(label) = assessment.label() {
        out.push_str(&format!(" — **{label}**"));
    }
    out.push_str("\n\n| run | started (UTC) | status | category | duration | attempts | script | version |\n");
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for e in entries {
        out.push_str(&format!(
            "| `{}` | {} | {} {} | {} | {} | {} | {} | {} |\n",
            short(&e.run_id),
            e.started_at.format("%Y-%m-%d %H:%M"),
            icon(&e.status),
            e.status,
            e.category.as_deref().unwrap_or(""),
            seconds(e.duration_ms),
            e.attempts,
            e.script_sha256.as_deref().map(|s| s.chars().take(8).collect::<String>()).unwrap_or_default(),
            e.installed_version.as_deref().unwrap_or("")
        ));
    }
    out
}

/// Differences between two runs.
pub fn render_diff(diff: &RunDiff) -> String {
    let mut out = format!(
        "## Diff `{}` → `{}` — {} changed, {} unchanged\n\n",
        short(&diff.from_run),
        short(&diff.to_run),
        diff.changes.len(),
        diff.unchanged
    );
    if diff.changes.is_empty() {
        out.push_str("No installer changed status.\n");
        return out;
    }
    out.push_str("| installer | change | before | after |\n|---|---|---|---|\n");
    for c in &diff.changes {
        let fmt = |s: &Option<String>, cat: &Option<String>| match (s, cat) {
            (Some(s), Some(c)) => format!("{} {s} ({c})", icon(s)),
            (Some(s), None) => format!("{} {s}", icon(s)),
            (None, _) => "—".to_string(),
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            c.installer,
            c.change,
            fmt(&c.before, &c.category_before),
            fmt(&c.after, &c.category_after)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::history::{diff_runs, History};
    use crate::reporting::{ResultPersister, RunHeader};
    use crate::runner::TestResult;

    #[test]
    fn run_table_lists_failures_first_with_notes() {
        let dir = tempfile::tempdir().unwrap();
        let persister = ResultPersister::new(dir.path());
        let mut bad = TestResult::new("zzz_bad").failed(1, "E: Unable to locate package foo");
        crate::runner::finalize_failure(&mut bad, None);
        let results = vec![TestResult::new("aaa_ok").passed(), bad];
        persister.persist_with_header(&results, &RunHeader::new("run-1234567890"), false).unwrap();
        let h = History::load(dir.path()).unwrap();
        let run = h.latest().unwrap();
        let md = render_run(run, &BTreeMap::new());
        assert!(md.starts_with("## AFSC run `run-1234` — 1 passed, 1 failed"), "{md}");
        let bad = md.find("zzz_bad").unwrap();
        let ok = md.find("aaa_ok").unwrap();
        assert!(bad < ok, "failures first:\n{md}");
        assert!(md.contains("| ❌ failed | dependency |"), "{md}");
        let d = diff_runs(run, run);
        let diff_md = render_diff(&d);
        assert!(diff_md.contains("0 changed, 2 unchanged"), "{diff_md}");
        let tl = render_timeline("zzz_bad", &h.installer_timeline("zzz_bad"), &h.assess("zzz_bad"));
        assert!(tl.contains("| `run-1234` |"), "{tl}");
    }
}
