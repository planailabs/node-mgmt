//! The axum transport for the control contract: the [`ControlApi`] trait every
//! backend implements + the [`router`] mapping `/api/*` to it. Behind the `server`
//! feature so the wasm SPA can depend on this crate for the `types` DTOs without
//! pulling axum/tokio. The launcher (real) and mock-server are the two impls, so
//! the routes + JSON shapes can't drift. See `standards/control-api.md`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

use crate::{ApiError, DownloadRequest, Info, Platforms, ServiceStatus, SetPlatforms, UpdateStatus};

/// The valid `{action}` values for `/api/services/{id}/{action}`. Validated once
/// here so no backend re-implements the check.
const SERVICE_ACTIONS: [&str; 3] = ["start", "stop", "restart"];

/// An upstream (llmfit) proxy reply: status code + raw body. The model browser is
/// an opaque pass-through (not part of our typed contract), so it stays bytes —
/// see the proxy exception in `standards/control-api.md`.
pub struct ProxyReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The backend behind `/api/*`. One impl is the real launcher, one is the mock.
/// RPITIT with `+ Send` so axum's handlers stay `Send` across `.await`.
pub trait ControlApi: Send + Sync + 'static {
    fn info(&self) -> impl Future<Output = Info> + Send;
    fn status(&self) -> impl Future<Output = Vec<ServiceStatus>> + Send;
    /// A fresh subscription to the live log feed (fanned out over SSE).
    fn subscribe_logs(&self) -> broadcast::Receiver<String>;
    /// Electron signalled its window is up (close the splash, etc.).
    fn ready(&self) -> impl Future<Output = ()> + Send;
    /// start | stop | restart a service ("all" = every service). The handler has
    /// already validated `action` against [`SERVICE_ACTIONS`].
    fn service_action(&self, svc: String, action: String) -> impl Future<Output = Result<(), String>> + Send;

    fn update_status(&self) -> impl Future<Output = UpdateStatus> + Send;
    fn update_check(&self) -> impl Future<Output = ()> + Send;
    fn update_apply(&self) -> impl Future<Output = ()> + Send;
    fn platforms(&self) -> impl Future<Output = Platforms> + Send;
    /// Persist the platform/feature selection. `features: None` keeps the current
    /// enabled set.
    fn set_platforms(&self, kept: Vec<String>, features: Option<Vec<String>>) -> impl Future<Output = ()> + Send;

    /// Proxy a GET/POST to the llmfit model-browser (real: reqwest; mock: fake).
    fn llmfit_get(&self, path: String) -> impl Future<Output = ProxyReply> + Send;
    fn llmfit_post(&self, path: String, body: String) -> impl Future<Output = ProxyReply> + Send;

    /// The stored daemon config as raw JSON (only the values the user set). Opaque
    /// JSON — the schema-driven editor in the SPA drives it, so it stays a
    /// `serde_json::Value` (same rationale as the llmfit pass-through). The real
    /// launcher proxies to the USB daemon's `/config`; the mock serves a sample.
    fn config(&self) -> impl Future<Output = serde_json::Value> + Send;
    /// The JSON Schema for the (reduced) USB config, driving the editor's form.
    fn config_schema(&self) -> impl Future<Output = serde_json::Value> + Send;
    /// Validate + persist + live-apply an edited config. `Ok` carries the
    /// daemon's `{ ok, applied, … }` reply; `Err` is a human-readable message.
    fn set_config(
        &self,
        body: serde_json::Value,
    ) -> impl Future<Output = Result<serde_json::Value, String>> + Send;
}

/// Build the `/api/*` router for any [`ControlApi`].
pub fn router<T: ControlApi>(state: Arc<T>) -> Router {
    Router::new()
        .route("/api/info", get(h_info::<T>))
        .route("/api/status", get(h_status::<T>))
        .route("/api/logs", get(h_logs::<T>))
        .route("/api/ready", post(h_ready::<T>))
        .route("/api/services/{name}/{action}", post(h_service::<T>))
        .route("/api/update/status", get(h_update_status::<T>))
        .route("/api/update/check", post(h_update_check::<T>))
        .route("/api/update/apply", post(h_update_apply::<T>))
        .route("/api/platforms", get(h_platforms::<T>).post(h_set_platforms::<T>))
        .route("/api/config", get(h_config::<T>).put(h_set_config::<T>))
        .route("/api/config/schema", get(h_config_schema::<T>))
        .route("/api/llmfit/models", get(h_llm_models::<T>))
        .route("/api/llmfit/installed", get(h_llm_installed::<T>))
        .route("/api/llmfit/download", post(h_llm_download::<T>))
        .route("/api/llmfit/download/{id}/status", get(h_llm_dl_status::<T>))
        .with_state(state)
}

// --- reads -----------------------------------------------------------------

async fn h_info<T: ControlApi>(State(s): State<Arc<T>>) -> Json<Info> {
    Json(s.info().await)
}
async fn h_status<T: ControlApi>(State(s): State<Arc<T>>) -> Json<Vec<ServiceStatus>> {
    Json(s.status().await)
}
async fn h_logs<T: ControlApi>(State(s): State<Arc<T>>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(s.subscribe_logs()).filter_map(|r| r.ok().map(|l| Ok(Event::default().data(l))));
    Sse::new(stream).keep_alive(KeepAlive::default())
}
async fn h_update_status<T: ControlApi>(State(s): State<Arc<T>>) -> Json<UpdateStatus> {
    Json(s.update_status().await)
}
async fn h_platforms<T: ControlApi>(State(s): State<Arc<T>>) -> Json<Platforms> {
    Json(s.platforms().await)
}
async fn h_config<T: ControlApi>(State(s): State<Arc<T>>) -> Json<serde_json::Value> {
    Json(s.config().await)
}
async fn h_config_schema<T: ControlApi>(State(s): State<Arc<T>>) -> Json<serde_json::Value> {
    Json(s.config_schema().await)
}
async fn h_set_config<T: ControlApi>(
    State(s): State<Arc<T>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    match s.set_config(body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, Json(ApiError::new(e))).into_response(),
    }
}

// --- commands (204 No Content on success) ----------------------------------

async fn h_ready<T: ControlApi>(State(s): State<Arc<T>>) -> StatusCode {
    s.ready().await;
    StatusCode::NO_CONTENT
}
async fn h_update_check<T: ControlApi>(State(s): State<Arc<T>>) -> StatusCode {
    s.update_check().await;
    StatusCode::NO_CONTENT
}
async fn h_update_apply<T: ControlApi>(State(s): State<Arc<T>>) -> StatusCode {
    s.update_apply().await;
    StatusCode::NO_CONTENT
}
async fn h_set_platforms<T: ControlApi>(State(s): State<Arc<T>>, Json(body): Json<SetPlatforms>) -> StatusCode {
    if !body.platforms.is_empty() {
        s.set_platforms(body.platforms, body.features).await;
    }
    StatusCode::NO_CONTENT
}
async fn h_service<T: ControlApi>(State(s): State<Arc<T>>, Path((name, action)): Path<(String, String)>) -> Response {
    if !SERVICE_ACTIONS.contains(&action.as_str()) {
        return (StatusCode::BAD_REQUEST, Json(ApiError::new(format!("unknown action: {action}")))).into_response();
    }
    match s.service_action(name, action).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError::new(e))).into_response(),
    }
}

// --- llmfit proxy (opaque pass-through; bytes, not typed) -------------------

fn proxy_response(r: ProxyReply) -> Response {
    let code = StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_GATEWAY);
    ([(header::CONTENT_TYPE, "application/json")], (code, r.body)).into_response()
}

async fn h_llm_models<T: ControlApi>(State(s): State<Arc<T>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let limit = q.get("limit").map(String::as_str).unwrap_or("12");
    let use_case = q.get("use_case").map(String::as_str).unwrap_or("general");
    let mut path = format!("/api/v1/models/top?limit={limit}&use_case={use_case}");
    if let Some(mf) = q.get("min_fit").filter(|v| !v.is_empty()) {
        path.push_str(&format!("&min_fit={mf}"));
    }
    proxy_response(s.llmfit_get(path).await)
}
async fn h_llm_installed<T: ControlApi>(State(s): State<Arc<T>>) -> Response {
    proxy_response(s.llmfit_get("/api/v1/installed".into()).await)
}
async fn h_llm_download<T: ControlApi>(State(s): State<Arc<T>>, Json(body): Json<DownloadRequest>) -> Response {
    let payload = json!({ "model": body.model, "runtime": "ollama" }).to_string();
    proxy_response(s.llmfit_post("/api/v1/download".into(), payload).await)
}
async fn h_llm_dl_status<T: ControlApi>(State(s): State<Arc<T>>, Path(id): Path<String>) -> Response {
    let enc = urlencode(&id);
    proxy_response(s.llmfit_get(format!("/api/v1/download/{enc}/status")).await)
}

/// Minimal percent-encoding for a path segment (download ids are short tokens).
pub fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
