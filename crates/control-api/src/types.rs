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

// UpdateState + UpdateStatus (the generic updater↔UI contract) now live in
// loader-manifest and are re-exported by this crate's lib.rs.

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
    /// The hermes dashboard URL — present when the "hermes" feature is enabled
    /// and the daemon reports its (effective) port. Drives the SPA's Hermes tab.
    #[serde(default)]
    pub hermes_url: Option<String>,
    /// The hermes web UI URL — present when the "hermes" feature is enabled and
    /// the daemon reports its (effective) port. Drives the SPA's Hermes Web UI tab.
    #[serde(default)]
    pub hermes_webui_url: Option<String>,
    pub ollama_port: u16,
    pub webui_port: u16,
    pub models_dir: String,
    pub data_dir: String,
    #[serde(default)]
    pub accel: Accel,
    #[serde(default)]
    pub gpu: Option<Gpu>,
}


/// Which platforms are kept on this USB + which exist, and which optional
/// features are enabled + which exist (`/api/platforms`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Platforms {
    pub kept: Vec<String>,
    pub available: Vec<String>,
    /// Enabled optional features (subset of `available_features`).
    #[serde(default)]
    pub features: Vec<String>,
    /// Every optional feature this build knows about.
    #[serde(default)]
    pub available_features: Vec<String>,
}

/// Request body for `POST /api/platforms`. `features: None` (key absent) leaves
/// the enabled features untouched — an older client can keep posting platforms
/// only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetPlatforms {
    pub platforms: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
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
