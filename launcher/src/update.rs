//! Auto-updater: read the local manifest, diff it against the remote one for the
//! kept platforms, and pre-download changed files to a local staging dir (each
//! sha-verified). Applying the staged update happens in `apply.rs` AFTER the
//! runtime shuts down. Manifest schema + diff + classification are shared with the
//! build tool via `plan-ai-manifest` (one source of truth).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use plan_ai_control_api::{UpdateState, UpdateStatus};
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
    pub fn set_applying(&self, done: u64, total: u64, done_bytes: u64, total_bytes: u64, rate_bps: u64) {
        let mut g = self.status.lock().unwrap();
        g.state = UpdateState::Applying;
        g.done = done;
        g.total = total;
        g.done_bytes = done_bytes;
        g.total_bytes = total_bytes;
        g.rate_bps = rate_bps;
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

fn downloading(done: u64, total: u64, done_bytes: u64, total_bytes: u64, rate_bps: u64, m: &Manifest) -> UpdateStatus {
    UpdateStatus {
        state: UpdateState::Downloading,
        done,
        total,
        done_bytes,
        total_bytes,
        rate_bps,
        version: m.version.clone(),
        commit: m.commit.clone(),
        message: None,
    }
}
fn failed(msg: String) -> UpdateStatus {
    UpdateStatus { state: UpdateState::Failed, message: Some(msg), ..UpdateStatus::idle() }
}

/// Human-readable byte size for logs + the splash throughput indicator (1.5 GiB, …).
pub(crate) fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut f = n as f64;
    let mut i = 0usize;
    while f >= 1024.0 && i < U.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{f:.1} {}", U[i]) }
}

/// Short commit for logs (first 8 chars; "?" when empty).
fn short(commit: &str) -> &str {
    if commit.is_empty() {
        "?"
    } else {
        &commit[..commit.len().min(8)]
    }
}

/// Check the remote manifest and pre-download the delta to staging (verified).
/// Sets `up` state throughout AND logs each step to the terminal (what server is
/// queried, the version diff, every file as it downloads, the totals). On success
/// stashes a `Pending` for apply. Bootstrap (no local manifest) falls out naturally —
/// the diff treats everything as new.
pub async fn check_and_predownload(up: Handle) {
    up.set(UpdateStatus { state: UpdateState::Checking, ..UpdateStatus::idle() });
    let url = update_url();
    let kept = read_platforms();
    crate::log(&format!("update: checking {url} for platform(s) [{}]", kept.join(", ")));
    let remote = match fetch_remote(&url).await {
        Ok(m) => m,
        Err(e) => {
            crate::log(&format!("update: couldn't reach update server {url}: {e}"));
            up.set(failed(format!("couldn't reach update server: {e}")));
            return;
        }
    };
    crate::log(&format!(
        "update: remote version {} (commit {}, built {}, {} files in manifest)",
        remote.version,
        short(&remote.commit),
        if remote.built_at.is_empty() { "?" } else { &remote.built_at },
        remote.files.len(),
    ));
    let local = load_local();
    match &local {
        Some(l) => crate::log(&format!("update: local version {} (commit {})", l.version, short(&l.commit))),
        None => crate::log("update: no local manifest — first-run bootstrap (everything is new)"),
    }
    let plan = manifest::diff(local.as_ref(), &remote, &kept);
    if plan.to_download.is_empty() && plan.to_delete.is_empty() {
        crate::log(&format!("update: already up to date (version {})", remote.version));
        up.set(UpdateStatus { state: UpdateState::Idle, version: remote.version, commit: remote.commit, ..UpdateStatus::idle() });
        return;
    }
    crate::log(&format!(
        "update: {} file(s) to download ({}), {} to remove",
        plan.to_download.len(),
        human_bytes(plan.total_bytes),
        plan.to_delete.len(),
    ));
    for p in &plan.to_delete {
        crate::log(&format!("update:   remove {p}"));
    }

    let staging = staging_dir(&remote.commit);
    let _ = std::fs::create_dir_all(&staging);
    // Keep the remote manifest beside the staged files so apply (and resume) can
    // re-derive the plan without the network.
    let _ = std::fs::write(staging.join("manifest.json"), remote.to_json_pretty());
    crate::log(&format!("update: staging into {}", staging.display()));

    let total = plan.to_download.len() as u64;
    let total_bytes = plan.total_bytes;
    up.set(downloading(0, total, 0, total_bytes, 0, &remote));
    let base = url.trim_end_matches('/').to_string();
    let mut done_bytes: u64 = 0;
    let start = Instant::now();
    for (i, e) in plan.to_download.iter().enumerate() {
        let dest = staging.join(&e.path);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file_url = format!("{base}/files/{}", e.path);
        let sha = e.sha256.clone().unwrap_or_default();
        let size = e.size.unwrap_or(0);
        crate::log(&format!("update: [{}/{}] downloading {} ({})", i + 1, total, e.path, human_bytes(size)));
        let mut res = net::download_to(&file_url, &dest, &sha, size, true).await;
        if let Err(err) = &res {
            crate::log(&format!("update: [{}/{}] {} interrupted ({err}); retrying fresh", i + 1, total, e.path));
            res = net::download_to(&file_url, &dest, &sha, size, false).await; // retry fresh
        }
        if let Err(err) = res {
            crate::log(&format!("update: [{}/{}] {} FAILED: {err}", i + 1, total, e.path));
            up.set(failed(format!("download {}: {err}", e.path)));
            return;
        }
        done_bytes += size;
        // Throughput for the UI indicator: cumulative bytes over elapsed wall time.
        let rate = (done_bytes as f64 / start.elapsed().as_secs_f64().max(0.001)) as u64;
        up.set(downloading(i as u64 + 1, total, done_bytes, total_bytes, rate, &remote));
    }

    crate::log(&format!(
        "update: staged {} file(s) ({}) — ready to apply version {} (commit {})",
        total, human_bytes(done_bytes), remote.version, short(&remote.commit),
    ));
    *up.pending.lock().unwrap() = Some(Pending { staging, remote: remote.clone(), kept });
    up.set(UpdateStatus {
        state: UpdateState::Ready,
        done: total,
        total,
        done_bytes,
        total_bytes,
        rate_bps: 0,
        version: remote.version,
        commit: remote.commit,
        message: None,
    });
}
