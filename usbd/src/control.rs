//! Loopback control + status API for the USB daemon. Bound to `[::1]` only; the
//! launcher proxies `/api/*` to it. Modeled on `usb/control.rs`, but the config
//! endpoints validate against the reduced [`UsbConfig`] and `/info` reports the
//! **effective** (resolved) service ports.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use tokio::sync::{RwLock, mpsc, oneshot, watch};

use crate::usb_config::UsbConfig;

// The `/info` and `/connection` response shapes are the shared launcher↔daemon
// contract (plan-ai-control-api, types only). `DaemonInfo` carries the resolved
// ports/URLs; `ConnectionStatus` the remote-management health. Re-exported so
// the rest of the daemon refers to them unqualified.
pub use plan_ai_control_api::{ConnectionStatus, DaemonInfo};

#[derive(Clone)]
pub struct ControlState {
    pub socket_path: PathBuf,
    pub offline: bool,
    pub loop_tx: mpsc::Sender<LoopMsg>,
    pub shutdown_tx: watch::Sender<bool>,
    /// Config read candidates (priority order) for `GET /config`.
    pub config_read: Vec<PathBuf>,
    /// Where `PUT /config` persists the edited config (JSON).
    pub config_write: PathBuf,
    /// Live effective ports / URLs, updated on config apply.
    pub info: Arc<RwLock<DaemonInfo>>,
    /// Live remote-management connection health, refreshed by the event loop.
    pub connection: Arc<RwLock<ConnectionStatus>>,
}

/// Messages to the stack loop (which owns the ServiceManager), answered via the
/// oneshot `resp`.
pub enum LoopMsg {
    /// Apply an edited UsbConfig at runtime: rebuild the desired service set,
    /// start newly-enabled, stop disabled, restart config-changed.
    ApplyConfig {
        cfg: Box<UsbConfig>,
        resp: oneshot::Sender<Result<(), String>>,
    },
}

#[derive(Deserialize)]
struct ServiceReq {
    name: String,
}

pub fn router(state: ControlState) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/info", get(info))
        .route("/connection", get(connection))
        .route("/usb/start", post(start))
        .route("/usb/stop", post(stop))
        .route("/usb/restart", post(restart))
        .route("/usb/shutdown", post(shutdown))
        .route("/config", get(get_config).put(put_config))
        .route("/config/schema", get(config_schema))
        .with_state(state)
}

async fn info(State(state): State<ControlState>) -> impl IntoResponse {
    let snap = state.info.read().await.clone();
    (StatusCode::OK, Json(snap))
}

async fn connection(State(state): State<ControlState>) -> impl IntoResponse {
    let snap = state.connection.read().await.clone();
    (StatusCode::OK, Json(snap))
}

/// GET /config — the config as actually stored on disk (only the values the
/// user set; `{}` when none).
async fn get_config(State(state): State<ControlState>) -> impl IntoResponse {
    let v = first_stored(&state.config_read).unwrap_or_else(|| serde_json::json!({}));
    (StatusCode::OK, Json(v))
}

fn first_stored(paths: &[PathBuf]) -> Option<serde_json::Value> {
    for p in paths {
        if !p.exists() {
            continue;
        }
        let contents = std::fs::read_to_string(p).ok()?;
        let is_json = p.extension().and_then(|e| e.to_str()) == Some("json");
        let mut v: serde_json::Value = if is_json {
            serde_json::from_str(&contents).ok()?
        } else {
            toml::from_str::<toml::Value>(&contents)
                .ok()
                .and_then(|t| serde_json::to_value(t).ok())?
        };
        mac_mgmt_common::config_migrate::migrate(&mut v);
        return Some(v);
    }
    None
}

/// GET /config/schema — JSON Schema for the **reduced** UsbConfig, so the shared
/// editor only renders the subset (ollama/openwebui/memvault/relay/server/…).
async fn config_schema() -> impl IntoResponse {
    let schema = schemars::schema_for!(UsbConfig);
    Json(serde_json::to_value(&schema).unwrap_or_default())
}

/// PUT /config — validate against UsbConfig, persist to `config_write` (JSON),
/// then apply live. Returns 422 on invalid config.
async fn put_config(
    State(state): State<ControlState>,
    Json(mut body): Json<serde_json::Value>,
) -> impl IntoResponse {
    mac_mgmt_common::config_migrate::migrate(&mut body);

    let cfg = match UsbConfig::from_json(&body) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({ "ok": false, "error": format!("invalid config: {e}") })),
            );
        }
    };

    let pretty = match serde_json::to_string_pretty(&body) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            );
        }
    };
    if let Some(parent) = state.config_write.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&state.config_write, pretty) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        );
    }

    let (tx, rx) = oneshot::channel();
    let applied = if state
        .loop_tx
        .send(LoopMsg::ApplyConfig {
            cfg: Box::new(cfg),
            resp: tx,
        })
        .await
        .is_ok()
    {
        rx.await.unwrap_or_else(|_| Err("apply dropped".into()))
    } else {
        Err("stack loop is gone".into())
    };

    let (note, applied_ok) = match &applied {
        Ok(()) => ("applied live", true),
        Err(e) => {
            tracing::warn!("config saved but live-apply failed: {e}");
            ("saved; restart to apply (live apply failed)", false)
        }
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "applied": applied_ok,
            "saved_to": state.config_write.display().to_string(),
            "note": note,
        })),
    )
}

async fn client(state: &ControlState) -> Result<mac_mgmt_services::Client, String> {
    mac_mgmt_services::Client::connect(&state.socket_path, Duration::from_secs(5))
        .await
        .map_err(|e| format!("cannot reach supervisor: {e}"))
}

async fn status(State(state): State<ControlState>) -> impl IntoResponse {
    let mut c = match client(&state).await {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "offline": state.offline, "error": e })),
            );
        }
    };
    let services = match c.list().await {
        Ok(list) => list
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "pid": s.pid,
                    "running": s.pid.is_some(),
                    "program": s.spec.as_ref().map(|sp| sp.program.clone()),
                })
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "offline": state.offline, "error": e.to_string() })),
            );
        }
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({ "offline": state.offline, "services": services })),
    )
}

fn svc_result(
    action: &str,
    name: &str,
    r: Result<(), String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match r {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "action": action, "name": name })),
        ),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "ok": false, "action": action, "name": name, "error": e })),
        ),
    }
}

async fn start(State(state): State<ControlState>, Json(req): Json<ServiceReq>) -> impl IntoResponse {
    let r = match client(&state).await {
        Ok(mut c) => c.start_service(&req.name).await.map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    svc_result("start", &req.name, r)
}

async fn stop(State(state): State<ControlState>, Json(req): Json<ServiceReq>) -> impl IntoResponse {
    let r = match client(&state).await {
        Ok(mut c) => c.stop_service(&req.name).await.map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    svc_result("stop", &req.name, r)
}

async fn restart(
    State(state): State<ControlState>,
    Json(req): Json<ServiceReq>,
) -> impl IntoResponse {
    let r = match client(&state).await {
        Ok(mut c) => c.restart_service(&req.name).await.map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    svc_result("restart", &req.name, r)
}

async fn shutdown(State(state): State<ControlState>) -> impl IntoResponse {
    let _ = state.shutdown_tx.send(true);
    (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
}
