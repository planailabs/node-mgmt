//! Thin wasm HTTP client to the launcher's same-origin control API (/api/*).
//! No CORS to worry about — the launcher serves both this SPA and the API.

use gloo_net::http::Request;
use serde::Deserialize;
use serde_json::Value;

/// A service row as the launcher reports it (/api/status).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub state: String,
}

pub async fn get_json(path: &str) -> Result<Value, String> {
    let resp = Request::get(path).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

pub async fn post_json(path: &str, body: Value) -> Result<Value, String> {
    let resp = Request::post(path)
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

/// Fire-and-check a control action (start/stop/restart); body-less POST.
pub async fn post_action(path: &str) -> Result<(), String> {
    let resp = Request::post(path).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(())
}

pub async fn info() -> Result<Value, String> {
    get_json("/api/info").await
}

pub async fn status() -> Result<Vec<Service>, String> {
    let v = get_json("/api/status").await?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}
