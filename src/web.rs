//! Web UI: serves the report as JSON plus a self-contained HTML page with
//! dual-axis charts rendered in SVG (no external dependencies).

use crate::bench::BenchmarkReport;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use std::sync::Arc;

const INDEX_HTML: &str = include_str!("../web/index.html");

pub struct WebState {
    pub report: BenchmarkReport,
}

pub fn router(report: BenchmarkReport) -> Router {
    let state = Arc::new(WebState { report });
    Router::new()
        .route("/", get(index))
        .route("/api/report", get(api_report))
        .route("/healthz", get(health))
        .with_state(state)
}

async fn index() -> impl IntoResponse {
    ([("Cache-Control", "no-store")], Html(INDEX_HTML))
}

async fn health() -> impl IntoResponse {
    "ok"
}

async fn api_report(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    match serde_json::to_string(&state.report) {
        Ok(json) => ([("Cache-Control", "no-store")], (StatusCode::OK, json)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialize error: {e}"),
        )
            .into_response(),
    }
}
