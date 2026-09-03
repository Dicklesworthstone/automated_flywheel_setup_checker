//! Metrics and health: `status --format prometheus`, `serve` endpoints, stale detection,
//! validate drift persistence. Everything is computed from persisted runs.

use super::support::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Minimal HTTP/1.1 client: returns (status, headers, body).
fn http(addr: &str, method: &str, path: &str) -> (u16, Vec<(String, String)>, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status: u16 = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers = lines
        .filter_map(|l| {
            l.split_once(':').map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
        })
        .collect();
    (status, headers, body.to_string())
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn without_age(text: &str) -> String {
    text.lines().filter(|l| !l.contains("age_seconds")).collect::<Vec<_>>().join("\n")
}

#[test]
fn prometheus_status_has_per_installer_series_and_is_stable() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    fx.add_dependency_failure("dep_fail_tool");
    let empty = stdout(&fx.run(&["status", "--format", "prometheus"]));
    assert!(empty.contains("afsc_health -1\n"), "no data yet: {empty}");
    fx.run(&["check", "--local", "--format", "jsonl"]);

    let a = fx.run(&["status", "--format", "prometheus"]);
    assert_eq!(a.status.code(), Some(0));
    assert_no_log_noise(&a);
    let text = stdout(&a);
    assert!(text.contains("afsc_tests_total_24h 2\n"), "{text}");
    assert!(text.contains("afsc_successful_tests_24h 1\n"), "{text}");
    assert!(text.contains("afsc_failed_tests_24h 1\n"), "{text}");
    assert!(text.contains("afsc_runs_24h 1\n"), "{text}");
    assert!(text.contains("afsc_health 1\n"), "{text}");
    assert!(text.contains("afsc_installer_status{installer=\"dep_fail_tool\"} 0\n"), "{text}");
    assert!(text.contains("afsc_installer_status{installer=\"good_tool\"} 1\n"), "{text}");
    assert!(text.contains("afsc_installer_attempts{installer=\"good_tool\"} 1\n"), "{text}");
    assert!(text.contains("afsc_run_last_timestamp "), "{text}");
    assert!(!text.contains("afsc_checksum_drift_total"), "no validate run yet: {text}");
    let b = stdout(&fx.run(&["status", "--format", "prometheus"]));
    assert_eq!(without_age(&text), without_age(&b), "deterministic apart from the age gauge");
}

#[test]
fn validate_check_hashes_persists_drift_for_metrics() {
    let mut fx = Fixture::new();
    fx.add_pass("good_tool");
    fx.add_installer_with_wrong_hash("drifted_tool", "#!/bin/bash\necho hi\n");
    let out = fx.run(&["validate", "--check-hashes", "--format", "json"]);
    assert_eq!(out.status.code(), Some(4), "drift exits 4");
    let report: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fx.home.join(".local/share/afsc/validate.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["mismatched"], serde_json::json!(["drifted_tool"]));
    assert_eq!(report["matched"], 1);
    let text = stdout(&fx.run(&["status", "--format", "prometheus"]));
    assert!(text.contains("afsc_checksum_drift_total 1\n"), "{text}");
    assert!(text.contains("afsc_validate_last_timestamp "), "{text}");
}

#[test]
fn serve_exposes_health_and_metrics_and_reports_stale_runs() {
    let mut fx = Fixture::new();
    fx.add_config_toml(
        "[monitoring]\nhealth_endpoint = true\nmetrics_enabled = true\nbind = \"127.0.0.1\"\nstale_after_seconds = 3600\n",
    );
    fx.add_pass("good_tool");
    fx.add_dependency_failure("dep_fail_tool");
    let run = fx.run(&["check", "--local", "--format", "jsonl"]);
    let run_id = jsonl_lines(&run)[0]["run_id"].as_str().unwrap().to_string();

    let mut child = fx
        .command(&["serve", "--health-port", "0"], &[], &[])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn serve");
    let stderr = child.stderr.take().unwrap();
    let mut reader = BufReader::new(stderr);
    let mut addr = String::new();
    let mut seen = String::new();
    for _ in 0..200 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        seen.push_str(&line);
        if let Some(rest) = line.trim().strip_prefix("listening=") {
            addr = rest.to_string();
            break;
        }
    }
    assert!(!addr.is_empty(), "serve did not print listening=<addr>:\n{seen}");
    assert!(addr.starts_with("127.0.0.1:"), "binds the configured address: {addr}");
    // Keep draining stderr in the background so the child never blocks on a full pipe.
    std::thread::spawn(move || {
        let mut sink = String::new();
        let _ = reader.read_to_string(&mut sink);
    });

    let result = std::panic::catch_unwind(|| {
        let (status, headers, body) = http(&addr, "GET", "/health");
        assert_eq!(status, 200, "{body}");
        assert_eq!(header(&headers, "cache-control"), Some("no-store"));
        let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(doc["status"], "ok");
        assert_eq!(doc["last_run_id"], run_id);
        assert_eq!(doc["total_tests_24h"], 2);
        assert_eq!(doc["failed_tests_24h"], 1);

        let (status, headers, body) = http(&addr, "GET", "/metrics");
        assert_eq!(status, 200);
        assert!(header(&headers, "content-type").unwrap().starts_with("text/plain; version=0.0.4"));
        assert!(body.contains("afsc_installer_status{installer=\"dep_fail_tool\"} 0\n"), "{body}");
        assert!(body.contains("afsc_health 1\n"), "{body}");

        let (status, headers, body) = http(&addr, "HEAD", "/metrics");
        assert_eq!(status, 200);
        assert!(body.is_empty(), "HEAD has no body");
        assert!(header(&headers, "content-length").unwrap().parse::<usize>().unwrap() > 0);

        assert_eq!(http(&addr, "GET", "/nope").0, 404);
        assert_eq!(http(&addr, "POST", "/health").0, 405);

        // Age the run past the stale threshold by rewriting its header timestamp.
        let results_dir = fx.home.join(".local/share/afsc/results");
        let file = std::fs::read_dir(&results_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .unwrap();
        let old = chrono::Utc::now() - chrono::Duration::hours(30);
        let rewritten: Vec<String> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|line| {
                let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
                if v["kind"] == "run" {
                    v["started_at"] = serde_json::json!(old.to_rfc3339());
                }
                v.to_string()
            })
            .collect();
        std::fs::write(&file, rewritten.join("\n") + "\n").unwrap();

        let (status, _, body) = http(&addr, "GET", "/health");
        assert_eq!(status, 503, "{body}");
        let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(doc["status"], "stale");
        assert!(doc["last_run_age_seconds"].as_i64().unwrap() > 3600);
        let (_, _, metrics) = http(&addr, "GET", "/metrics");
        assert!(metrics.contains("afsc_health 0\n"), "{metrics}");
        assert!(metrics.contains("afsc_runs_24h 0\n"), "{metrics}");
    });
    let _ = child.kill();
    let _ = child.wait();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn serve_refuses_disabled_endpoints_and_bad_bind() {
    let fx = Fixture::new();
    let out = fx.run(&["serve", "--health-port", "0"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "endpoints disabled is a config error: {}",
        stderr(&out)
    );
    let mut fx = Fixture::new();
    fx.add_config_toml("[monitoring]\nhealth_endpoint = true\nbind = \"nowhere\"\n");
    let out = fx.run(&["serve", "--health-port", "0"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("not an IP address"), "{}", stderr(&out));
}
