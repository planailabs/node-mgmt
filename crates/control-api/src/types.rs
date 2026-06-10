//! The plan.ai control-plane contract: every request/response DTO and state enum
//! the SPA and its backends exchange over `/api/*`. Serde-only (no axum/tokio) so
//! the wasm SPA, the launcher, and the mock all depend on the SAME types — the JSON
//! shapes cannot drift. See `standards/control-api.md`.

use serde::{Deserialize, Serialize};

/// Lifecycle of a supervised service (`/api/status`). The wire strings live here
/// and nowhere else — UI labels/colours `match` on the enum, never re-spell them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Ready,
    Starting,
    Stopped,
    Error,
}

/// Lifecycle of the auto-updater (`/api/update/status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    Idle,
    Checking,
    Downloading,
    Ready,
    Applying,
    Failed,
}

/// A supervised service row (`/api/status`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub id: String,
    pub name: String,
    pub state: ServiceState,
}

/// Hardware-acceleration summary (part of [`Info`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Accel {
    #[serde(default)]
    pub flavour: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Detected GPU/host facts (part of [`Info`]); every field is optional — a probe
/// may report none of them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    #[serde(default)]
    pub gpu_name: Option<String>,
    #[serde(default)]
    pub gpu_vram_gb: Option<f64>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub total_ram_gb: Option<f64>,
    #[serde(default)]
    pub cpu_name: Option<String>,
}

/// Runtime facts the dashboard shows (`/api/info`). `llmfit_url` being present is
/// also how the SPA knows the model-browser tab is available.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Info {
    pub webui_url: String,
    #[serde(default)]
    pub llmfit_url: Option<String>,
    pub ollama_port: u16,
    pub webui_port: u16,
    pub models_dir: String,
    pub data_dir: String,
    #[serde(default)]
    pub accel: Accel,
    #[serde(default)]
    pub gpu: Option<Gpu>,
}

/// The auto-updater's state (`/api/update/status`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateStatus {
    pub state: UpdateState,
    /// Files done / total (the unit the progress bar uses).
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub total: u64,
    /// Bytes done / total + current throughput (bytes/sec) — the UI's throughput
    /// indicator. Set during Downloading (network) and Applying (copy-onto-drive);
    /// 0 otherwise.
    #[serde(default)]
    pub done_bytes: u64,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub rate_bps: u64,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub message: Option<String>,
}

impl UpdateStatus {
    pub fn idle() -> Self {
        UpdateStatus {
            state: UpdateState::Idle,
            done: 0,
            total: 0,
            done_bytes: 0,
            total_bytes: 0,
            rate_bps: 0,
            version: String::new(),
            commit: String::new(),
            message: None,
        }
    }
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self::idle()
    }
}

/// Which platforms are kept on this USB + which exist (`/api/platforms`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Platforms {
    pub kept: Vec<String>,
    pub available: Vec<String>,
}

/// Request body for `POST /api/platforms`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetPlatforms {
    pub platforms: Vec<String>,
}

/// Request body for `POST /api/llmfit/download`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub model: String,
}

/// The uniform error body for any non-2xx response (`application/json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

impl ApiError {
    pub fn new(message: impl Into<String>) -> Self {
        ApiError { error: message.into() }
    }
}
