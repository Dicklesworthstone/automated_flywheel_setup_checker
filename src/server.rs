//! Minimal HTTP endpoints for health and metrics exposure.
//!
//! Every request recomputes from the data directory (results files + `validate.json`), so the
//! endpoints never drift from `status --format prometheus`. `/health` returns
//! `monitoring.stale_status_code` (503 by default) when the last run is older than
//! `monitoring.stale_after_seconds`; `no_data` is 200 so a freshly deployed host is not paged.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::config::MonitoringConfig;
use crate::reporting::{HealthState, MetricsReport};

type ResponseBody = Full<Bytes>;

const JSON_CONTENT_TYPE: &str = "application/json";
const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Clone, Debug)]
struct MonitoringServerConfig {
    health_enabled: bool,
    metrics_enabled: bool,
    bind: IpAddr,
    listen_port: u16,
    data_dir: PathBuf,
    stale_after_seconds: u64,
    stale_status_code: u16,
}

impl MonitoringServerConfig {
    fn from_config(
        config: &MonitoringConfig,
        health_port_override: Option<u16>,
        metrics_port_override: Option<u16>,
        data_dir: PathBuf,
    ) -> Result<Self> {
        if !config.health_endpoint && !config.metrics_enabled {
            bail!("monitoring endpoints are disabled in config; enable [monitoring].health_endpoint and/or [monitoring].metrics_enabled");
        }

        let configured_port =
            if config.health_endpoint { config.health_port } else { config.metrics_port };
        let listen_port = health_port_override.or(metrics_port_override).unwrap_or(configured_port);
        let bind: IpAddr =
            config.bind.trim().parse().with_context(|| {
                format!("[monitoring].bind {:?} is not an IP address", config.bind)
            })?;

        Ok(Self {
            health_enabled: config.health_endpoint,
            metrics_enabled: config.metrics_enabled,
            bind,
            listen_port,
            data_dir,
            stale_after_seconds: config.stale_after_seconds,
            stale_status_code: config.stale_status_code,
        })
    }
}

/// Serve `/health` and `/metrics` computed from `data_dir` (results files, validate.json).
///
/// The bound address is logged and, when the configured port is 0, also printed to stderr as
/// `listening=<addr>` so callers can discover the ephemeral port.
pub async fn serve_monitoring(
    config: &MonitoringConfig,
    health_port_override: Option<u16>,
    metrics_port_override: Option<u16>,
    data_dir: PathBuf,
) -> Result<()> {
    let server_config = MonitoringServerConfig::from_config(
        config,
        health_port_override,
        metrics_port_override,
        data_dir,
    )?;
    let addr = SocketAddr::from((server_config.bind, server_config.listen_port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind monitoring server to {addr}"))?;
    let bound = listener.local_addr().unwrap_or(addr);

    info!(
        address = %bound,
        health_enabled = server_config.health_enabled,
        metrics_enabled = server_config.metrics_enabled,
        data_dir = %server_config.data_dir.display(),
        "Monitoring server listening"
    );
    if server_config.listen_port == 0 {
        eprintln!("listening={bound}");
    }

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("Monitoring server received shutdown signal");
                return Ok(());
            }
            accept_result = listener.accept() => {
                let (stream, peer_addr) = accept_result?;
                let config = server_config.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| handle_request(req, config.clone()));
                    if let Err(error) = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        warn!(peer = %peer_addr, error = %error, "Monitoring connection failed");
                    }
                });
            }
        }
    }
}

async fn handle_request(
    request: Request<Incoming>,
    config: MonitoringServerConfig,
) -> Result<Response<ResponseBody>, Infallible> {
    let head_only = request.method() == Method::HEAD;
    let method_ok = matches!(request.method(), &Method::GET | &Method::HEAD);
    let mut response = match (method_ok, request.uri().path()) {
        (true, "/health") if config.health_enabled => health_response(&config),
        (true, "/metrics") if config.metrics_enabled => metrics_response(&config),
        (true, "/health") | (true, "/metrics") => {
            text_response(StatusCode::NOT_FOUND, TEXT_CONTENT_TYPE, "endpoint disabled")
        }
        (true, _) => text_response(StatusCode::NOT_FOUND, TEXT_CONTENT_TYPE, "not found"),
        (false, _) => {
            text_response(StatusCode::METHOD_NOT_ALLOWED, TEXT_CONTENT_TYPE, "method not allowed")
        }
    };
    if head_only {
        // Same status and headers, empty body (Content-Length still describes the GET body).
        let len = response.headers().get(CONTENT_LENGTH).cloned();
        let (mut parts, _) = response.into_parts();
        if let Some(len) = len {
            parts.headers.insert(CONTENT_LENGTH, len);
        }
        response = Response::from_parts(parts, Full::new(Bytes::new()));
    }
    Ok(response)
}

fn compute(config: &MonitoringServerConfig) -> Result<MetricsReport> {
    MetricsReport::from_data_dir(&config.data_dir, chrono::Utc::now(), config.stale_after_seconds)
}

fn health_response(config: &MonitoringServerConfig) -> Response<ResponseBody> {
    match compute(config) {
        Ok(report) => {
            let status = health_status(&report, config.stale_status_code);
            json_response(status, report.health_json())
        }
        Err(error) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "status": "error", "error": error.to_string() }),
        ),
    }
}

fn health_status(report: &MetricsReport, stale_status_code: u16) -> StatusCode {
    match report.health {
        HealthState::Stale => {
            StatusCode::from_u16(stale_status_code).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
        }
        HealthState::Ok | HealthState::NoData => StatusCode::OK,
    }
}

fn metrics_response(config: &MonitoringServerConfig) -> Response<ResponseBody> {
    match compute(config) {
        Ok(report) => {
            text_response(StatusCode::OK, PROMETHEUS_CONTENT_TYPE, report.to_prometheus())
        }
        Err(error) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            TEXT_CONTENT_TYPE,
            format!("failed to compute metrics: {error}"),
        ),
    }
}

/// Health document for a data dir (used by `status` and tests without a listener).
pub fn health_document(data_dir: &Path, stale_after_seconds: u64) -> Result<serde_json::Value> {
    Ok(MetricsReport::from_data_dir(data_dir, chrono::Utc::now(), stale_after_seconds)?
        .health_json())
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response<ResponseBody> {
    let body = serde_json::to_vec(&body).expect("serializing JSON response should never fail");
    response(status, JSON_CONTENT_TYPE, Bytes::from(body))
}

fn text_response(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<Bytes>,
) -> Response<ResponseBody> {
    response(status, content_type, body.into())
}

fn response(status: StatusCode, content_type: &'static str, body: Bytes) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header(CACHE_CONTROL, "no-store")
        .header(CONTENT_LENGTH, body.len())
        .body(Full::new(body))
        .expect("constructing monitoring response should never fail")
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("registering SIGTERM handler should succeed");
        let _ = signal.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::{ResultPersister, RunHeader};
    use crate::runner::TestResult;

    #[test]
    fn server_config_requires_enabled_endpoints_and_a_valid_bind() {
        let config = MonitoringConfig::default();
        let error = MonitoringServerConfig::from_config(&config, None, None, PathBuf::from("/x"))
            .unwrap_err();
        assert!(error.to_string().contains("monitoring endpoints are disabled"));

        let bad_bind = MonitoringConfig {
            health_endpoint: true,
            bind: "localhost".into(),
            ..Default::default()
        };
        let error = MonitoringServerConfig::from_config(&bad_bind, None, None, PathBuf::from("/x"))
            .unwrap_err();
        assert!(error.to_string().contains("not an IP address"), "{error}");
    }

    #[test]
    fn server_config_uses_metrics_port_when_only_metrics_enabled() {
        let config = MonitoringConfig {
            health_endpoint: false,
            health_port: 8080,
            metrics_enabled: true,
            metrics_port: 9191,
            bind: "127.0.0.1".into(),
            ..Default::default()
        };
        let server_config =
            MonitoringServerConfig::from_config(&config, None, None, PathBuf::from("/x")).unwrap();
        assert_eq!(server_config.listen_port, 9191);
        assert_eq!(server_config.bind, IpAddr::from([127, 0, 0, 1]));
        assert!(!server_config.health_enabled);
        assert!(server_config.metrics_enabled);
        let overridden =
            MonitoringServerConfig::from_config(&config, Some(0), None, PathBuf::from("/x"))
                .unwrap();
        assert_eq!(overridden.listen_port, 0);
    }

    #[test]
    fn health_document_reports_no_data_then_ok_then_stale() {
        let dir = tempfile::tempdir().unwrap();
        let doc = health_document(dir.path(), 100).unwrap();
        assert_eq!(doc["status"], "no_data");

        let persister = ResultPersister::new(dir.path().join("results"));
        let header = RunHeader::new("run-1");
        persister.persist_with_header(&[TestResult::new("a").passed()], &header, false).unwrap();
        let doc = health_document(dir.path(), 100).unwrap();
        assert_eq!(doc["status"], "ok");
        assert_eq!(doc["total_tests_24h"], 1);

        let mut old = RunHeader::new("run-0");
        old.started_at = chrono::Utc::now() - chrono::Duration::hours(30);
        // Newer file name but older header: the header timestamp decides.
        let persister2 = ResultPersister::new(dir.path().join("results2"));
        persister2.persist_with_header(&[TestResult::new("a").passed()], &old, false).unwrap();
        let doc = health_document(&dir.path().join("nested"), 100).unwrap();
        assert_eq!(doc["status"], "no_data");
        let report = MetricsReport::from_data_dir(
            dir.path(),
            chrono::Utc::now() + chrono::Duration::seconds(200),
            100,
        )
        .unwrap();
        assert_eq!(report.health, HealthState::Stale);
        assert_eq!(health_status(&report, 503), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(health_status(&report, 299), StatusCode::from_u16(299).unwrap());
    }
}
