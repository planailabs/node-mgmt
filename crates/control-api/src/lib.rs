//! The control-plane HTTP surface the Dioxus SPA talks to, defined ONCE so the
//! launcher (real) and the mock-server (dev preview) can't drift. Implement
//! [`ControlApi`] and hand it to [`router`]; both get identical /api/* routes +
//! JSON shapes.

use std::future::Future;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

/// A supervised service row (/api/status).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub id: String,
    pub name: String,
    pub state: String, // ready | starting | stopped | error
}

/// The auto-updater's state (/api/update/status).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// idle | checking | downloading | ready | applying | failed
    pub state: String,
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub message: Option<String>,
}

impl UpdateStatus {
    pub fn idle() -> Self {
        UpdateStatus { state: "idle".into(), done: 0, total: 0, version: String::new(), commit: String::new(), message: None }
    }
}

/// Which platforms are kept on this USB + which exist (/api/platforms).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Platforms {
    pub kept: Vec<String>,
    pub available: Vec<String>,
}

/// An upstream (llmfit) proxy reply: status code + raw JSON body.
pub struct ProxyReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The backend behind /api/*. One impl is the real launcher, one is the mock.
/// RPITIT with `+ Send` so axum's handlers stay Send across `.await`.
pub trait ControlApi: Send + Sync + 'static {
    fn info(&self) -> impl Future<Output = Value> + Send;
    fn status(&self) -> impl Future<Output = Vec<ServiceStatus>> + Send;
    /// A fresh subscription to the live log feed (fanned out over SSE).
    fn subscribe_logs(&self) -> broadcast::Receiver<String>;
    /// Electron signalled its window is up (close the splash, etc.).
    fn ready(&self) -> impl Future<Output = ()> + Send;
    /// start | stop | restart a service ("all" = every service).
    fn service_action(&self, svc: String, action: String) -> impl Future<Output = Result<(), String>> + Send;

    fn update_status(&self) -> impl Future<Output = UpdateStatus> + Send;
    fn update_check(&self) -> impl Future<Output = ()> + Send;
    fn update_apply(&self) -> impl Future<Output = ()> + Send;
    fn platforms(&self) -> impl Future<Output = Platforms> + Send;
    fn set_platforms(&self, kept: Vec<String>) -> impl Future<Output = ()> + Send;

    /// Proxy a GET/POST to the llmfit model-browser (real: reqwest; mock: fake).
    fn llmfit_get(&self, path: String) -> impl Future<Output = ProxyReply> + Send;
    fn llmfit_post(&self, path: String, body: String) -> impl Future<Output = ProxyReply> + Send;
}

/// Build the /api/* router for any [`ControlApi`].
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
        .route("/api/llmfit/models", get(h_llm_models::<T>))
        .route("/api/llmfit/installed", get(h_llm_installed::<T>))
        .route("/api/llmfit/download", post(h_llm_download::<T>))
        .route("/api/llmfit/download/{id}/status", get(h_llm_dl_status::<T>))
        .with_state(state)
}

async fn h_info<T: ControlApi>(State(s): State<Arc<T>>) -> Json<Value> {
    Json(s.info().await)
}
async fn h_status<T: ControlApi>(State(s): State<Arc<T>>) -> Json<Vec<ServiceStatus>> {
    Json(s.status().await)
}
async fn h_logs<T: ControlApi>(State(s): State<Arc<T>>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(s.subscribe_logs()).filter_map(|r| r.ok().map(|l| Ok(Event::default().data(l))));
    Sse::new(stream).keep_alive(KeepAlive::default())
}
async fn h_ready<T: ControlApi>(State(s): State<Arc<T>>) -> impl IntoResponse {
    s.ready().await;
    (StatusCode::OK, "ok")
}
async fn h_service<T: ControlApi>(State(s): State<Arc<T>>, Path((name, action)): Path<(String, String)>) -> impl IntoResponse {
    match s.service_action(name, action).await {
        Ok(()) => (StatusCode::OK, "ok".to_string()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
async fn h_update_status<T: ControlApi>(State(s): State<Arc<T>>) -> Json<UpdateStatus> {
    Json(s.update_status().await)
}
async fn h_update_check<T: ControlApi>(State(s): State<Arc<T>>) -> impl IntoResponse {
    s.update_check().await;
    (StatusCode::OK, "ok")
}
async fn h_update_apply<T: ControlApi>(State(s): State<Arc<T>>) -> impl IntoResponse {
    s.update_apply().await;
    (StatusCode::OK, "ok")
}
async fn h_platforms<T: ControlApi>(State(s): State<Arc<T>>) -> Json<Platforms> {
    Json(s.platforms().await)
}
async fn h_set_platforms<T: ControlApi>(State(s): State<Arc<T>>, Json(body): Json<Value>) -> impl IntoResponse {
    let kept: Vec<String> = body
        .get("platforms")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if !kept.is_empty() {
        s.set_platforms(kept).await;
    }
    (StatusCode::OK, "ok")
}

fn proxy_response(r: ProxyReply) -> axum::response::Response {
    let code = StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_GATEWAY);
    ([(header::CONTENT_TYPE, "application/json")], (code, r.body)).into_response()
}

async fn h_llm_models<T: ControlApi>(State(s): State<Arc<T>>, Query(q): Query<HashMap<String, String>>) -> axum::response::Response {
    let limit = q.get("limit").map(String::as_str).unwrap_or("12");
    let use_case = q.get("use_case").map(String::as_str).unwrap_or("general");
    let mut path = format!("/api/v1/models/top?limit={limit}&use_case={use_case}");
    if let Some(mf) = q.get("min_fit").filter(|v| !v.is_empty()) {
        path.push_str(&format!("&min_fit={mf}"));
    }
    proxy_response(s.llmfit_get(path).await)
}
async fn h_llm_installed<T: ControlApi>(State(s): State<Arc<T>>) -> axum::response::Response {
    proxy_response(s.llmfit_get("/api/v1/installed".into()).await)
}
async fn h_llm_download<T: ControlApi>(State(s): State<Arc<T>>, Json(body): Json<Value>) -> axum::response::Response {
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or_default();
    let payload = json!({ "model": model, "runtime": "ollama" }).to_string();
    proxy_response(s.llmfit_post("/api/v1/download".into(), payload).await)
}
async fn h_llm_dl_status<T: ControlApi>(State(s): State<Arc<T>>, Path(id): Path<String>) -> axum::response::Response {
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
