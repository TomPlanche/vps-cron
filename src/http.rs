//! A small read-only status server.
//!
//! It answers the question you actually have about an unattended VPS: is the
//! manager alive, and did last night's jobs do what they were supposed to?
//! Everything it exposes is read-only, so binding it to localhost and reaching
//! it over an SSH tunnel is enough.
//!
//! - `GET /health` - liveness, plus a count of jobs currently failing
//! - `GET /jobs` - every configured job with its next run and last result
//! - `GET /jobs/{name}` - one job
//! - `GET /runs?job={name}&limit={n}` - recent runs from the history

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

use crate::history::History;
use crate::scheduler::SharedStatus;

/// Largest number of history rows one request may ask for.
const MAX_RUN_LIMIT: u32 = 500;
/// Default number of history rows when the request does not say.
const DEFAULT_RUN_LIMIT: u32 = 50;

/// Shared state for the request handlers.
#[derive(Clone)]
struct AppState {
    status: SharedStatus,
    history: History,
}

/// Binds the status server and serves until the process exits.
pub async fn serve(addr: &str, status: SharedStatus, history: History) -> Result<()> {
    let socket: SocketAddr = addr
        .parse()
        .with_context(|| format!("HTTP_ADDR is not a valid address: '{addr}'"))?;

    let app = Router::new()
        .route("/health", get(health))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{name}", get(get_job))
        .route("/runs", get(list_runs))
        .with_state(AppState { status, history });

    let listener = tokio::net::TcpListener::bind(socket)
        .await
        .with_context(|| format!("Failed to bind the status server to {socket}"))?;

    info!(%socket, "Status server listening");

    axum::serve(listener, app)
        .await
        .context("The status server stopped")
}

/// Liveness, with a count of jobs whose most recent run was a problem.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let status = state.status.read().await;

    let failing: Vec<&str> = status
        .values()
        .filter(|job| {
            job.last_run
                .as_ref()
                .is_some_and(|run| run.outcome.is_problem())
        })
        .map(|job| job.name.as_str())
        .collect();

    Json(json!({
        "status": if failing.is_empty() { "ok" } else { "degraded" },
        "jobs": status.len(),
        "enabled": status.values().filter(|job| job.enabled).count(),
        "running": status.values().filter(|job| job.running).count(),
        "failing": failing,
    }))
}

/// Every configured job, enabled or not.
async fn list_jobs(State(state): State<AppState>) -> impl IntoResponse {
    let status = state.status.read().await;
    let jobs: Vec<_> = status.values().cloned().collect();
    Json(json!({ "jobs": jobs }))
}

/// One job by name.
async fn get_job(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let status = state.status.read().await;

    status
        .get(&name)
        .map(|job| Json(job.clone()).into_response())
        .ok_or_else(|| ApiError::not_found(format!("No job named '{name}'")))
}

/// Query parameters accepted by `GET /runs`.
#[derive(Debug, Deserialize)]
struct RunsQuery {
    /// Restrict the listing to one job.
    job: Option<String>,
    /// How many rows to return.
    limit: Option<u32>,
}

/// Recent runs from the history, newest first.
async fn list_runs(
    State(state): State<AppState>,
    Query(query): Query<RunsQuery>,
) -> Result<Response, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_RUN_LIMIT).clamp(1, MAX_RUN_LIMIT);

    let runs = state
        .history
        .recent(query.job, limit)
        .await
        .map_err(|error| ApiError::internal(format!("{error:#}")))?;

    Ok(Json(json!({ "runs": runs })).into_response())
}

/// An error rendered as a JSON body with a status code.
#[derive(Debug, Serialize)]
struct ApiError {
    #[serde(skip)]
    status: StatusCode,
    error: String,
}

impl ApiError {
    /// A 404 with an explanation.
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error: message.into(),
        }
    }

    /// A 500 with an explanation.
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.error }))).into_response()
    }
}
