//! Localhost HTTP server (phase 3): serves the rust-embedded Dioxus SPA + a
//! control API the SPA calls (status, logs SSE, start/stop/restart). Electron
//! (thin webview, phase 5) loads the returned URL.

use crate::config;
use axum::{
    extract::{Path as AxPath, State},
    http::{header, StatusCode, Uri},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
    routing::{get, post},
    Router,
};
use mac_mgmt_services::{protocol::Notification, Client};
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Mutex};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

/// The Dioxus SPA, embedded at compile time (placeholder until phase 4 builds it).
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
        "ollama_port": config::OLLAMA_PORT,
        "webui_port": config::WEBUI_PORT,
        "gpu": s.gpu_json.as_deref().and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok()),
    }))
}

async fn status(State(s): State<AppState>) -> Json<serde_json::Value> {
    let list = { s.client.lock().await.list().await.unwrap_or_default() };
    Json(serde_json::to_value(list).unwrap_or_else(|_| json!([])))
}

async fn control(State(s): State<AppState>, AxPath((name, action)): AxPath<(String, String)>) -> impl IntoResponse {
    let mut c = s.client.lock().await;
    let r = match action.as_str() {
        "start" => c.start_service(&name).await,
        "stop" => c.stop_service(&name).await,
        "restart" => c.restart_service(&name).await,
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    match r {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn logs_sse(State(s): State<AppState>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(s.logs.subscribe())
        .filter_map(|r| r.ok().map(|line| Ok(Event::default().data(line))));
    Sse::new(stream).keep_alive(KeepAlive::default())
}
