//! Localhost HTTP server (phase 3): serves the rust-embedded Dioxus SPA + a
//! control API the SPA calls (status, logs SSE, start/stop/restart, llmfit model
//! browser). Electron (thin webview, phase 5) loads the returned URL.

use crate::{config, control, paths, proxy};
use axum::{
    extract::{Path as AxPath, Query, State},
    http::{header, StatusCode, Uri},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
    routing::{get, post},
    Router,
};
use mac_mgmt_services::{
    protocol::{Notification, ServiceStatus},
    Client,
};
use serde_json::json;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Mutex};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

/// The Dioxus SPA, embedded at compile time (built into spa/ by scripts/build-spa.sh
/// or the flake before the launcher compiles).
#[derive(rust_embed::RustEmbed)]
#[folder = "spa/"]
struct Spa;

#[derive(Clone)]
struct AppState {
    client: Arc<Mutex<Client>>,
    logs: broadcast::Sender<String>,
    gpu_json: Option<String>,
    llmfit_url: Option<String>,
    webui_url: String,
}

/// Start the control server. Returns the base URL; the server runs on a task.
pub async fn run_server(client: Client, port: u16) -> anyhow::Result<String> {
    let (logs_tx, _) = broadcast::channel::<String>(512);
    let state = AppState {
        client: Arc::new(Mutex::new(client)),
        logs: logs_tx,
        gpu_json: std::env::var("PLANAI_GPU_JSON").ok(),
        llmfit_url: std::env::var("PLANAI_LLMFIT_URL").ok(),
        webui_url: config::webui_url(),
    };

    // Drain supervisor notifications → fan out to SSE subscribers.
    {
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                {
                    let mut c = st.client.lock().await;
                    while let Some(n) = c.try_recv_notification() {
                        let _ = st.logs.send(fmt_notif(&n));
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
    }

    let app = Router::new()
        .route("/api/info", get(info))
        .route("/api/status", get(status))
        .route("/api/logs", get(logs_sse))
        .route("/api/services/{name}/{action}", post(control))
        // llmfit model browser (proxied to `llmfit serve`, same-origin for the SPA)
        .route("/api/llmfit/models", get(llmfit_models))
        .route("/api/llmfit/installed", get(llmfit_installed))
        .route("/api/llmfit/download", post(llmfit_download))
        .route("/api/llmfit/download/{id}/status", get(llmfit_download_status))
        .fallback(static_handler)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let url = format!("http://127.0.0.1:{port}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(url)
}

fn fmt_notif(n: &Notification) -> String {
    match n {
        Notification::Log { name, line, .. } => format!("[{name}] {line}"),
        Notification::Crashed { name, exit_code } => format!("[{name}] crashed (exit {exit_code:?})"),
    }
}

async fn static_handler(uri: Uri) -> impl IntoResponse {
    let p = uri.path().trim_start_matches('/');
    let p = if p.is_empty() { "index.html" } else { p };
    let serve = |path: &str| {
        Spa::get(path).map(|f| {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref().to_string())], f.data.into_owned()).into_response()
        })
    };
    serve(p)
        .or_else(|| serve("index.html")) // SPA fallback
        .unwrap_or_else(|| (StatusCode::NOT_FOUND, "not found").into_response())
}

async fn info(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "webui_url": s.webui_url,
        "llmfit_url": s.llmfit_url,
        "ollama_port": config::ollama_port(),
        "webui_port": config::webui_port(),
        "models_dir": paths::models_dir().to_string_lossy(),
        "data_dir": paths::data_dir().to_string_lossy(),
        "accel": {
            "flavour": std::env::var("PLANAI_OLLAMA_FLAVOUR").ok(),
            "reason": std::env::var("PLANAI_OLLAMA_REASON").ok(),
            "llmfit_url": s.llmfit_url,
        },
        "gpu": s.gpu_json.as_deref().and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok()),
    }))
}

/// Map a supervisor ServiceStatus + an OS-level health probe to the dashboard's
/// {ready, starting, stopped} states.
async fn service_state(st: Option<&ServiceStatus>, health_url: &str) -> &'static str {
    match st {
        None => "stopped",
        Some(s) if s.stopped => "stopped",
        Some(s) if s.pid.is_none() => "starting",
        Some(_) => {
            if control::http_ok(health_url).await {
                "ready"
            } else {
                "starting"
            }
        }
    }
}

async fn status(State(s): State<AppState>) -> Json<serde_json::Value> {
    let list = { s.client.lock().await.list().await.unwrap_or_default() };
    let by = |name: &str| list.iter().find(|x| x.name == name);
    let ollama = service_state(by("ollama"), &config::ollama_health_url()).await;
    let webui = service_state(by("open-webui"), &config::webui_health_url()).await;
    Json(json!([
        { "id": "ollama", "name": "Ollama", "state": ollama },
        { "id": "webui", "name": "Open-WebUI", "state": webui },
    ]))
}

async fn control(State(s): State<AppState>, AxPath((name, action)): AxPath<(String, String)>) -> impl IntoResponse {
    // The dashboard speaks ids (ollama/webui); the supervisor registered
    // "ollama"/"open-webui". start/stop with no name act on all services.
    let svc = match name.as_str() {
        "webui" => "open-webui",
        other => other,
    };
    let mut c = s.client.lock().await;
    let names: Vec<String> = if svc == "all" {
        c.list().await.map(|l| l.into_iter().map(|x| x.name).collect()).unwrap_or_default()
    } else {
        vec![svc.to_string()]
    };
    for n in &names {
        let r = match action.as_str() {
            "start" => c.start_service(n).await,
            "stop" => c.stop_service(n).await,
            "restart" => c.restart_service(n).await,
            _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
        };
        if let Err(e) = r {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    (StatusCode::OK, "ok").into_response()
}

async fn logs_sse(State(s): State<AppState>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(s.logs.subscribe())
        .filter_map(|r| r.ok().map(|line| Ok(Event::default().data(line))));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── llmfit model-browser proxy ─────────────────────────────────────────────

fn llmfit_base(s: &AppState) -> Result<String, axum::response::Response> {
    s.llmfit_url
        .clone()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "model browser unavailable (llmfit not running)").into_response())
}

fn proxy_json(r: anyhow::Result<proxy::ProxyResponse>) -> axum::response::Response {
    match r {
        Ok(resp) => {
            let code = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY);
            ([(header::CONTENT_TYPE, "application/json")], (code, resp.body)).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("llmfit proxy: {e}")).into_response(),
    }
}

async fn llmfit_models(State(s): State<AppState>, Query(q): Query<HashMap<String, String>>) -> axum::response::Response {
    let base = match llmfit_base(&s) { Ok(b) => b, Err(r) => return r };
    let limit = q.get("limit").map(String::as_str).unwrap_or("12");
    let use_case = q.get("use_case").map(String::as_str).unwrap_or("general");
    let mut path = format!("/api/v1/models/top?limit={limit}&use_case={use_case}");
    if let Some(mf) = q.get("min_fit").filter(|v| !v.is_empty()) {
        path.push_str(&format!("&min_fit={mf}"));
    }
    proxy_json(proxy::get(&base, &path).await)
}

async fn llmfit_installed(State(s): State<AppState>) -> axum::response::Response {
    let base = match llmfit_base(&s) { Ok(b) => b, Err(r) => return r };
    proxy_json(proxy::get(&base, "/api/v1/installed").await)
}

async fn llmfit_download(State(s): State<AppState>, Json(body): Json<serde_json::Value>) -> axum::response::Response {
    let base = match llmfit_base(&s) { Ok(b) => b, Err(r) => return r };
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or_default();
    let payload = json!({ "model": model, "runtime": "ollama" }).to_string();
    proxy_json(proxy::post(&base, "/api/v1/download", &payload).await)
}

async fn llmfit_download_status(State(s): State<AppState>, AxPath(id): AxPath<String>) -> axum::response::Response {
    let base = match llmfit_base(&s) { Ok(b) => b, Err(r) => return r };
    let enc = urlencode(&id);
    proxy_json(proxy::get(&base, &format!("/api/v1/download/{enc}/status")).await)
}

/// Minimal percent-encoding for a path segment (download ids are short tokens).
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
