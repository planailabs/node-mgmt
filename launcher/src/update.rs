//! Auto-updater: read the local manifest, diff it against the remote one for the
//! kept platforms, and pre-download changed files to a local staging dir (each
//! sha-verified). Applying the staged update happens in `apply.rs` AFTER the
//! runtime shuts down. Manifest schema + diff + classification are shared with the
//! build tool via `plan-ai-manifest` (one source of truth).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use plan_ai_control_api::UpdateStatus;
use plan_ai_manifest::{self as manifest, Manifest};

use crate::{cache_root, net, paths};

/// A verified, staged update awaiting apply.
pub struct Pending {
    pub staging: PathBuf,
    pub remote: Manifest,
    pub kept: Vec<String>,
}

/// Shared updater state (status for the SPA + the staged pending update for apply).
pub struct Updater {
    status: Mutex<UpdateStatus>,
    pub pending: Mutex<Option<Pending>>,
}

pub type Handle = Arc<Updater>;

impl Updater {
    pub fn new() -> Handle {
        Arc::new(Updater { status: Mutex::new(UpdateStatus::idle()), pending: Mutex::new(None) })
    }
    pub fn status(&self) -> UpdateStatus {
        self.status.lock().unwrap().clone()
    }
    fn set(&self, s: UpdateStatus) {
        *self.status.lock().unwrap() = s;
    }
    pub fn set_applying(&self, done: u64, total: u64) {
        let mut g = self.status.lock().unwrap();
        g.state = "applying".into();
        g.done = done;
        g.total = total;
    }
}

fn local_manifest_path() -> PathBuf {
    paths::portable_root().join("update.json")
}
fn platforms_path() -> PathBuf {
    paths::portable_root().join("platforms.json")
}

pub fn load_local() -> Option<Manifest> {
    std::fs::read_to_string(local_manifest_path()).ok().and_then(|s| Manifest::from_json(&s).ok())
}

/// The update server URL: from the local manifest, else the hardcoded default.
pub fn update_url() -> String {
    load_local()
        .map(|m| m.update_url)
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| manifest::DEFAULT_UPDATE_URL.to_string())
}

/// Kept platforms for this USB. Creates platforms.json with the current platform
/// on first run (returning whether it had to create it).
pub fn read_platforms() -> Vec<String> {
    if let Ok(s) = std::fs::read_to_string(platforms_path()) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            let k: Vec<String> = v
                .get("platforms")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if !k.is_empty() {
                return k;
            }
        }
    }
    let cur = vec![manifest::current_platform().to_string()];
    write_platforms(&cur);
    cur
}

pub fn platforms_exists() -> bool {
    platforms_path().exists()
}

pub fn write_platforms(kept: &[String]) {
    let _ = std::fs::write(platforms_path(), serde_json::json!({ "platforms": kept }).to_string());
}

fn staging_dir(commit: &str) -> PathBuf {
    let key = if commit.is_empty() { "bootstrap" } else { commit };
    cache_root().join("update").join(key)
}

async fn fetch_remote(url: &str) -> anyhow::Result<Manifest> {
    let base = url.trim_end_matches('/');
    let json = net::get_string(&format!("{base}/manifest.json")).await?;
    Ok(Manifest::from_json(&json)?)
}

fn downloading(done: u64, total: u64, m: &Manifest) -> UpdateStatus {
    UpdateStatus { state: "downloading".into(), done, total, version: m.version.clone(), commit: m.commit.clone(), message: None }
}
fn failed(msg: String) -> UpdateStatus {
    UpdateStatus { state: "failed".into(), message: Some(msg), ..UpdateStatus::idle() }
}

/// Check the remote manifest and pre-download the delta to staging (verified).
/// Sets `up` state throughout; on success stashes a `Pending` for apply. Bootstrap
/// (no local manifest) falls out naturally — the diff treats everything as new.
pub async fn check_and_predownload(up: Handle) {
    up.set(UpdateStatus { state: "checking".into(), ..UpdateStatus::idle() });
    let url = update_url();
    let kept = read_platforms();
    let remote = match fetch_remote(&url).await {
        Ok(m) => m,
        Err(e) => {
            up.set(failed(format!("couldn't reach update server: {e}")));
            return;
        }
    };
    let local = load_local();
    let plan = manifest::diff(local.as_ref(), &remote, &kept);
    if plan.to_download.is_empty() && plan.to_delete.is_empty() {
        up.set(UpdateStatus { state: "idle".into(), version: remote.version, commit: remote.commit, ..UpdateStatus::idle() });
        return;
    }

    let staging = staging_dir(&remote.commit);
    let _ = std::fs::create_dir_all(&staging);
    // Keep the remote manifest beside the staged files so apply (and resume) can
    // re-derive the plan without the network.
    let _ = std::fs::write(staging.join("manifest.json"), remote.to_json_pretty());

    let total = plan.to_download.len() as u64;
    up.set(downloading(0, total, &remote));
    let base = url.trim_end_matches('/').to_string();
    for (i, e) in plan.to_download.iter().enumerate() {
        let dest = staging.join(&e.path);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file_url = format!("{base}/files/{}", e.path);
        let sha = e.sha256.clone().unwrap_or_default();
        let size = e.size.unwrap_or(0);
        let mut res = net::download_to(&file_url, &dest, &sha, size, true).await;
        if res.is_err() {
            res = net::download_to(&file_url, &dest, &sha, size, false).await; // retry fresh
        }
        if let Err(err) = res {
            up.set(failed(format!("download {}: {err}", e.path)));
            return;
        }
        up.set(downloading(i as u64 + 1, total, &remote));
    }

    *up.pending.lock().unwrap() = Some(Pending { staging, remote: remote.clone(), kept });
    up.set(UpdateStatus { state: "ready".into(), done: total, total, version: remote.version, commit: remote.commit, message: None });
}
