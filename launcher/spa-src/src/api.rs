//! Thin wasm HTTP client to the launcher's same-origin control API (/api/*). No
//! CORS — the launcher serves both this SPA and the API. Request/response types
//! come from the shared `plan-ai-control-api` crate (the same the backends
//! implement), so the SPA can't drift from the contract. See
//! `standards/control-api.md`.
//!
//! Only the llmfit model-browser proxy (`/api/llmfit/*`) is read as untyped
//! `Value` — it pass-through-proxies an upstream we don't own (standards §2).

use gloo_net::http::{Request, Response};
use serde::de::DeserializeOwned;
use serde_json::Value;

pub use plan_ai_control_api::{ConnectionStatus, Info, Platforms, ServiceState, ServiceStatus, UpdateState, UpdateStatus};

/// Pull the `{"error": ...}` body off a non-2xx response, else the HTTP status.
async fn err_message(resp: Response) -> String {
    let status = resp.status();
    match resp.json::<plan_ai_control_api::ApiError>().await {
        Ok(e) => e.error,
        Err(_) => format!("HTTP {status}"),
    }
}

/// Typed GET into a contract DTO.
pub async fn get<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let resp = Request::get(path).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(err_message(resp).await);
    }
    resp.json::<T>().await.map_err(|e| e.to_string())
}

/// Fire a command (POST). Succeeds on any 2xx (incl. the contract's 204) WITHOUT
/// reading the body; surfaces `ApiError` on failure.
pub async fn command(path: &str) -> Result<(), String> {
    let resp = Request::post(path).send().await.map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err(err_message(resp).await)
    }
}

/// Fire a command with a JSON body.
pub async fn command_json(path: &str, body: Value) -> Result<(), String> {
    let resp = Request::post(path).json(&body).map_err(|e| e.to_string())?.send().await.map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err(err_message(resp).await)
    }
}

// --- llmfit proxy only: untyped upstream JSON (standards §2 exception) ------

pub async fn get_json(path: &str) -> Result<Value, String> {
    get::<Value>(path).await
}

pub async fn post_json(path: &str, body: Value) -> Result<Value, String> {
    let resp = Request::post(path).json(&body).map_err(|e| e.to_string())?.send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(err_message(resp).await);
    }
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

// --- typed contract endpoints ----------------------------------------------

pub async fn info() -> Result<Info, String> {
    get("/api/info").await
}

pub async fn status() -> Result<Vec<ServiceStatus>, String> {
    get("/api/status").await
}

// --- config plane: schema-driven, opaque Value (standards §2 exception) -----

/// The stored config the editor edits.
pub async fn config() -> Result<Value, String> {
    get_json("/api/config").await
}

/// The JSON Schema that drives the editor's form (the reduced UsbConfig schema).
pub async fn config_schema() -> Result<Value, String> {
    get_json("/api/config/schema").await
}

/// Persist + live-apply an edited config (PUT). The daemon validates against
/// UsbConfig and returns `{ok, applied, …}`, or `422 {error}` on failure.
pub async fn set_config(body: Value) -> Result<Value, String> {
    let resp = Request::put("/api/config")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(err_message(resp).await);
    }
    resp.json::<Value>().await.map_err(|e| e.to_string())
}
