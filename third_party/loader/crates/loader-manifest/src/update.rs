//! The auto-updater's wire DTOs — the generic contract between the loader-core
//! updater (which writes the status as it downloads/applies) and a consumer's
//! control-plane API + UI (which polls it). Moved out of the plan.ai control-api so
//! it's shared with the framework; serde-only, so a wasm UI can depend on it directly.

use serde::{Deserialize, Serialize};

/// The auto-updater's state (`/api/update/status`).
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

/// The auto-updater's status (`/api/update/status`).
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
