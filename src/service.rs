//! Long-running mode: run a pass every interval (plus jitter) and optionally
//! serve read-only status endpoints.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use rand::RngExt;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::error::{Error, Result};
use crate::runner::{Output, RegionReport, RunOptions, Runner};
use crate::server::ServerRegion;

#[derive(Default)]
struct Status {
    last_run: Option<DateTime<Utc>>,
    reports: BTreeMap<ServerRegion, RegionReport>,
}

type Shared = Arc<RwLock<Status>>;

/// The delay before the next pass: `interval` plus a uniform `0..=jitter`.
#[must_use]
pub fn next_delay(interval_secs: u64, jitter_secs: u64) -> Duration {
    let jitter = if jitter_secs == 0 {
        0
    } else {
        rand::rng().random_range(0..=jitter_secs)
    };
    Duration::from_secs(interval_secs + jitter)
}

/// Runs until SIGINT/SIGTERM.
pub async fn serve(runner: Runner) -> Result<()> {
    let runner = Arc::new(runner);
    let status: Shared = Arc::default();
    let regions = runner.config().enabled_regions();
    if regions.is_empty() {
        return Err(Error::Config("no enabled region has a url".into()));
    }
    let listen = runner.config().service.listen.trim().to_owned();
    if !listen.is_empty() {
        let listener = tokio::net::TcpListener::bind(&listen)
            .await
            .map_err(|e| Error::Config(format!("service.listen {listen}: {e}")))?;
        info!(%listen, "Serving status endpoints");
        let app = router(Arc::clone(&status));
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!(error = %e, "Status server stopped");
            }
        });
    }
    let schedule = runner.config().schedule.clone();
    info!(
        regions = ?regions,
        interval_secs = schedule.interval_secs,
        jitter_secs = schedule.jitter_secs,
        "Scheduler started"
    );
    let mut shutdown = std::pin::pin!(shutdown_signal());
    loop {
        let pass = async {
            let reports = runner
                .run_once(
                    &regions,
                    RunOptions {
                        force: false,
                        publish: true,
                    },
                )
                .await;
            let mut guard = status.write().await;
            guard.last_run = Some(Utc::now());
            for report in reports {
                guard.reports.insert(report.region, report);
            }
        };
        tokio::select! {
            () = pass => {}
            () = &mut shutdown => break,
        }
        let delay = next_delay(schedule.interval_secs, schedule.jitter_secs);
        info!(next_in_secs = delay.as_secs(), "Pass finished");
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = &mut shutdown => break,
        }
    }
    info!("Shutting down");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

fn router(status: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/identity", get(identities))
        .route("/v1/identity/{region}", get(identity))
        .with_state(status)
}

async fn health(State(status): State<Shared>) -> Response {
    let guard = status.read().await;
    let degraded = guard.reports.values().any(|r| !r.is_ok());
    let regions: BTreeMap<_, _> = guard
        .reports
        .iter()
        .map(|(region, report)| {
            (
                *region,
                json!({
                    "ok": report.is_ok(),
                    "appVersion": report.identity.as_ref().map(|i| &i.app_version),
                    "checkedAt": report.checked_at,
                    "error": report.error,
                    "publishErrors": report.publish_errors,
                }),
            )
        })
        .collect();
    Json(json!({
        "status": if guard.last_run.is_none() { "starting" } else if degraded { "degraded" } else { "ok" },
        "lastRun": guard.last_run,
        "regions": regions,
    }))
    .into_response()
}

async fn identities(State(status): State<Shared>) -> Response {
    let guard = status.read().await;
    let reports: Vec<_> = guard.reports.values().cloned().collect();
    Json(Output::from_reports(&reports)).into_response()
}

async fn identity(State(status): State<Shared>, Path(region): Path<String>) -> Response {
    let Ok(region) = region.parse::<ServerRegion>() else {
        return (StatusCode::NOT_FOUND, "unknown region").into_response();
    };
    let guard = status.read().await;
    match guard.reports.get(&region).and_then(|r| r.identity.clone()) {
        Some(identity) => Json(identity).into_response(),
        None => (StatusCode::NOT_FOUND, "no identity yet").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_stays_within_the_jitter_window() {
        for _ in 0..100 {
            let delay = next_delay(600, 120).as_secs();
            assert!((600..=720).contains(&delay));
        }
        assert_eq!(next_delay(30, 0), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn status_endpoints_report_the_last_pass() {
        let status: Shared = Arc::default();
        let app = router(Arc::clone(&status));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let get = |path: &str| client.get(format!("http://{addr}{path}")).send();

        let health: serde_json::Value = get("/health").await.unwrap().json().await.unwrap();
        assert_eq!(health["status"], "starting");
        assert_eq!(get("/v1/identity/jp").await.unwrap().status(), 404);
        assert_eq!(get("/v1/identity/xx").await.unwrap().status(), 404);

        {
            let mut guard = status.write().await;
            guard.last_run = Some(Utc::now());
            guard.reports.insert(
                ServerRegion::En,
                RegionReport {
                    region: ServerRegion::En,
                    extracted: true,
                    identity: None,
                    error: Some("probe failed".into()),
                    publish_errors: Vec::new(),
                    checked_at: Utc::now(),
                },
            );
        }
        let health: serde_json::Value = get("/health").await.unwrap().json().await.unwrap();
        assert_eq!(health["status"], "degraded");
        assert_eq!(health["regions"]["en"]["error"], "probe failed");
        let all: serde_json::Value = get("/v1/identity").await.unwrap().json().await.unwrap();
        assert_eq!(all["errors"]["en"], "probe failed");
    }
}
