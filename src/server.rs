//! Minimal HTTP endpoints for health and metrics exposure.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::config::MonitoringConfig;
use crate::reporting::{MetricsExporter, MetricsSnapshot};

type ResponseBody = Full<Bytes>;

const JSON_CONTENT_TYPE: &str = "application/json";
const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Clone, Debug)]
struct MonitoringServerConfig {
    health_enabled: bool,
    metrics_enabled: bool,
    listen_port: u16,
    snapshot_path: PathBuf,
}

impl MonitoringServerConfig {
    fn from_config(
        config: &MonitoringConfig,
        health_port_override: Option<u16>,
        metrics_port_override: Option<u16>,
        snapshot_path: PathBuf,
    ) -> Result<Self> {
        if !config.health_endpoint && !config.metrics_enabled {
            bail!("monitoring endpoints are disabled in config; enable [monitoring].health_endpoint and/or [monitoring].metrics_enabled");
        }

        let configured_port =
            if config.health_endpoint { config.health_port } else { config.metrics_port };
        let listen_port = health_port_override.or(metrics_port_override).unwrap_or(configured_port);

        Ok(Self {
            health_enabled: config.health_endpoint,
            metrics_enabled: config.metrics_enabled,
            listen_port,
            snapshot_path,
        })
    }
}

/// Serve `/health` and `/metrics` from the persisted metrics snapshot at `snapshot_path`.
pub async fn serve_monitoring(
    config: &MonitoringConfig,
    health_port_override: Option<u16>,
    metrics_port_override: Option<u16>,
    snapshot_path: PathBuf,
) -> Result<()> {
    let server_config = MonitoringServerConfig::from_config(
        config,
        health_port_override,
        metrics_port_override,
        snapshot_path,
    )?;
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, server_config.listen_port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind monitoring server to {addr}"))?;

    info!(
        address = %addr,
        health_enabled = server_config.health_enabled,
        metrics_enabled = server_config.metrics_enabled,
        "Monitoring server listening"
    );

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
    let response = match (request.method(), request.uri().path()) {
        (&Method::GET, "/health") if config.health_enabled => {
            health_response(&config.snapshot_path)
        }
        (&Method::GET, "/metrics") if config.metrics_enabled => {
            metrics_response(&config.snapshot_path)
        }
        (&Method::GET, "/health") | (&Method::GET, "/metrics") => {
            text_response(StatusCode::NOT_FOUND, TEXT_CONTENT_TYPE, "endpoint disabled")
        }
        (&Method::GET, _) => text_response(StatusCode::NOT_FOUND, TEXT_CONTENT_TYPE, "not found"),
        _ => text_response(StatusCode::METHOD_NOT_ALLOWED, TEXT_CONTENT_TYPE, "method not allowed"),
    };

    Ok(response)
}

fn health_response(snapshot_path: &Path) -> Response<ResponseBody> {
    match load_snapshot(snapshot_path) {
        Ok(Some(snapshot)) => json_response(StatusCode::OK, build_health_payload(Some(&snapshot))),
        Ok(None) => json_response(StatusCode::OK, build_health_payload(None)),
        Err(error) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "status": "error",
                "error": error.to_string(),
            }),
        ),
    }
}

fn metrics_response(snapshot_path: &Path) -> Response<ResponseBody> {
    match load_snapshot(snapshot_path) {
        Ok(Some(snapshot)) => {
            text_response(StatusCode::OK, PROMETHEUS_CONTENT_TYPE, render_metrics(&snapshot))
        }
        Ok(None) => text_response(
            StatusCode::OK,
            PROMETHEUS_CONTENT_TYPE,
            render_metrics(&MetricsSnapshot::default()),
        ),
        Err(error) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            TEXT_CONTENT_TYPE,
            format!("failed to load metrics snapshot: {error}"),
        ),
    }
}

fn load_snapshot(path: &Path) -> Result<Option<MetricsSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }

    let mut snapshot = MetricsSnapshot::load(path)?;
    snapshot.reset_if_stale();
    Ok(Some(snapshot))
}

fn build_health_payload(snapshot: Option<&MetricsSnapshot>) -> serde_json::Value {
    match snapshot {
        Some(snapshot) => json!({
            "status": "ok",
            "last_run": snapshot.last_test,
            "last_success": snapshot.last_success,
            "last_failure": snapshot.last_failure,
            "success_rate_24h": snapshot.success_rate_24h,
            "total_tests_24h": snapshot.total_tests_24h,
            "successful_tests_24h": snapshot.successful_tests_24h,
            "total_remediations_24h": snapshot.total_remediations_24h,
            "uptime_seconds": snapshot.uptime_seconds,
            "snapshot_time": snapshot.snapshot_time,
        }),
        None => json!({ "status": "no_data" }),
    }
}

fn render_metrics(snapshot: &MetricsSnapshot) -> String {
    MetricsExporter::from_snapshot("afsc", snapshot).export()
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
    use chrono::{TimeZone, Utc};

    #[test]
    fn test_server_config_requires_enabled_endpoints() {
        let config = MonitoringConfig::default();
        let error = MonitoringServerConfig::from_config(&config, None, None, MetricsSnapshot::default_path()).unwrap_err();
        assert!(error.to_string().contains("monitoring endpoints are disabled"));
    }

    #[test]
    fn test_server_config_uses_metrics_port_when_only_metrics_enabled() {
        let config = MonitoringConfig {
            health_endpoint: false,
            health_port: 8080,
            metrics_enabled: true,
            metrics_port: 9191,
            ..Default::default()
        };

        let server_config = MonitoringServerConfig::from_config(&config, None, None, MetricsSnapshot::default_path()).unwrap();
        assert_eq!(server_config.listen_port, 9191);
        assert!(!server_config.health_enabled);
        assert!(server_config.metrics_enabled);
    }

    #[test]
    fn test_build_health_payload_without_snapshot_reports_no_data() {
        let payload = build_health_payload(None);
        assert_eq!(payload["status"], "no_data");
    }

    #[test]
    fn test_build_health_payload_with_snapshot_reports_metrics() {
        let snapshot = MetricsSnapshot {
            last_test: Some(Utc.with_ymd_and_hms(2026, 4, 7, 3, 0, 0).unwrap()),
            success_rate_24h: 0.75,
            total_tests_24h: 8,
            successful_tests_24h: 6,
            total_remediations_24h: 2,
            uptime_seconds: 42,
            ..Default::default()
        };

        let payload = build_health_payload(Some(&snapshot));
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["total_tests_24h"], 8);
        assert_eq!(payload["successful_tests_24h"], 6);
        assert_eq!(payload["uptime_seconds"], 42);
        assert!(payload["last_run"].is_string());
    }
}
