//! Auto-updater: read the local manifest, diff it against the remote one for the
//! kept platforms, and pre-download changed files to a local staging dir (each
//! sha-verified). Applying the staged update happens in `apply.rs` AFTER the
//! runtime shuts down. Manifest schema + diff + classification are shared with the
//! build tool via `plan-ai-manifest` (one source of truth).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use plan_ai_control_api::{UpdateState, UpdateStatus};
use plan_ai_manifest::{self as manifest, Manifest, Selection};

use crate::{cache_root, net, paths};

/// A verified, staged update awaiting apply.
pub struct Pending {
    pub staging: PathBuf,
    pub remote: Manifest,
    pub sel: Selection,
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

/// The drive's selection (kept platforms + enabled features) from platforms.json.
/// Creates it with the current platform + default features on first run. A file
/// without a `features` key (older drives) deserializes with the default feature
/// set (see manifest::Selection).
pub fn read_selection() -> Selection {
    if let Ok(s) = std::fs::read_to_string(platforms_path()) {
        if let Ok(sel) = serde_json::from_str::<Selection>(&s) {
            if !sel.platforms.is_empty() {
                return sel;
            }
        }
    }
    let cur = Selection::current_platform_default();
    write_selection(&cur);
    cur
}

pub fn platforms_exists() -> bool {
    platforms_path().exists()
}

pub fn write_selection(sel: &Selection) {
    let _ = std::fs::write(platforms_path(), serde_json::to_string(sel).unwrap_or_default());
}

fn staging_dir(commit: &str) -> PathBuf {
    let key = if commit.is_empty() { "bootstrap" } else { commit };
    cache_root().join("update").join(key)
}

/// Persistent "update in flight" marker on the PENDRIVE (portable root), pointing
/// at the staging folder. Unlike the in-memory `Pending` (lost on exit) and the
/// cache-local staging (may be cleared per-machine), this lives on the drive that
/// travels with the user — so a download/apply interrupted by a crash, power loss,
/// or a busy-mount teardown is detected and retried/resumed on the next launch
/// (`apply::resume_if_interrupted`). Written when the delta is staged + ready;
/// cleared once applied (or when already up to date).
pub fn pending_marker_path() -> PathBuf {
    paths::portable_root().join(".update-pending.json")
}

/// Record a staged-and-ready update on the pendrive so a restart applies it.
pub fn write_pending_marker(commit: &str, version: &str, staging: &Path) {
    let _ = std::fs::write(
        pending_marker_path(),
        serde_json::json!({
            "commit": commit,
            "version": version,
            "staging": staging.to_string_lossy(),
        })
        .to_string(),
    );
}

/// Clear the pendrive marker (update applied, or nothing to do).
pub fn clear_pending_marker() {
    let _ = std::fs::remove_file(pending_marker_path());
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

/// Drive-relative on-disk artifact of a manifest entry: a zip-component's unpacked
/// `target` folder, else the file's own path. None for dir entries (nothing on disk
/// to verify — they're just `mkdir`s).
pub(crate) fn entry_artifact(e: &manifest::Entry) -> Option<&str> {
    if e.is_dir() {
        None
    } else if e.is_zip() {
        e.target.as_deref()
    } else {
        Some(e.path.as_str())
    }
}

/// Is an entry's artifact actually present under `root` (the drive)? A zip-component
/// counts as present only when its unpacked target dir exists AND is non-empty (a
/// half-wiped/failed unpack shouldn't pass). Unsafe/odd paths are treated as present
/// (never our business to re-fetch).
pub(crate) fn artifact_present(root: &Path, e: &manifest::Entry) -> bool {
    let Some(rel) = entry_artifact(e) else { return true };
    if !manifest::is_safe_path(rel) {
        return true;
    }
    let p = root.join(rel);
    if e.is_zip() {
        std::fs::read_dir(&p).map(|mut d| d.next().is_some()).unwrap_or(false)
    } else {
        p.exists()
    }
}

/// The local manifest to diff the remote against, adjusted for what's PHYSICALLY on
/// the drive (the manifest-only `diff` can't see disk state). When a wanted
/// component's artifact is missing — a re-added platform, a re-enabled feature, a
/// deleted/corrupted file — plain diffing would say "up to date" (the local manifest
/// still lists it at the matching sha), so:
///
///   - same version+commit as the remote (no update available): HEAL — return the
///     local manifest WITHOUT the missing entries, so the diff re-downloads exactly
///     those and leaves the intact components alone. Safe: everything is fetched at
///     the one version the drive already runs.
///   - different version: None (→ a FULL re-fetch of every wanted component at the
///     remote version). A selective re-fetch would pull the gone file at the REMOTE
///     version while its on-disk siblings stay at the LOCAL version — a mismatched,
///     half-updated component set that may not even work together.
pub(crate) fn local_for_plan(remote: &Manifest, sel: &Selection, root: &Path) -> Option<Manifest> {
    let local = load_local()?;
    let missing: Vec<String> = remote
        .files
        .iter()
        .filter(|e| e.wanted_by(sel) && !artifact_present(root, e))
        .map(|e| e.path.clone())
        .collect();
    if missing.is_empty() {
        return Some(local);
    }
    if local.version == remote.version && local.commit == remote.commit {
        crate::log(&format!(
            "update: {} wanted component(s) missing on the drive at the current version — \
             healing (re-downloading just those)",
            missing.len(),
        ));
        let mut healed = local;
        healed.files.retain(|e| !missing.iter().any(|m| m == &e.path));
        return Some(healed);
    }
    crate::log(
        "update: a wanted component is missing AND the remote has a new version — doing a \
         FULL re-fetch (all components to the remote version; a partial heal could mix versions)",
    );
    None
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
    let sel = read_selection();
    crate::log(&format!(
        "update: checking {url} for platform(s) [{}], feature(s) [{}]",
        sel.platforms.join(", "),
        sel.features.join(", "),
    ));
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
    // Diff against the local manifest — adjusted for missing on-disk artifacts (a
    // heal of just those at the same version, or a full re-fetch across versions;
    // see local_for_plan).
    let local = local_for_plan(&remote, &sel, &paths::portable_root());
    match &local {
        Some(l) => crate::log(&format!("update: local version {} (commit {})", l.version, short(&l.commit))),
        None => crate::log("update: no local manifest (or repairing) — fetching every wanted component"),
    }
    let plan = manifest::diff(local.as_ref(), &remote, &sel);
    if plan.to_download.is_empty() && plan.to_delete.is_empty() && plan.to_wipe_dirs.is_empty() {
        crate::log(&format!("update: already up to date (version {})", remote.version));
        clear_pending_marker();
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
        // Resume across launches: a file fully staged by a prior (interrupted) run
        // has been renamed off its `.part`, so download_to would re-fetch it whole.
        // Skip it when it's already present and its hash matches — only partial or
        // missing files are (re)downloaded (download_to resumes those via `.part`).
        if !sha.is_empty() && dest.exists() && manifest::sha256_file(&dest).map(|g| g == sha).unwrap_or(false) {
            crate::log(&format!("update: [{}/{}] {} already staged — skipping", i + 1, total, e.path));
            done_bytes += size;
            let rate = (done_bytes as f64 / start.elapsed().as_secs_f64().max(0.001)) as u64;
            up.set(downloading(i as u64 + 1, total, done_bytes, total_bytes, rate, &remote));
            continue;
        }
        crate::log(&format!("update: [{}/{}] downloading {} ({})", i + 1, total, e.path, human_bytes(size)));
        // Steady byte-level progress: download_to reports cumulative bytes for THIS
        // file; we publish `done_bytes (finished files) + this file's bytes` so the
        // bar advances within a file, not just once per file. Throttled (~150ms) and
        // monotonic so the lock isn't hammered per chunk and the bar never rewinds.
        let progress = {
            let up = up.clone();
            let remote = remote.clone();
            let base = done_bytes;
            let mut last = std::time::Instant::now();
            let mut peak = 0u64;
            move |file_bytes: u64| {
                let file_bytes = file_bytes.min(size).max(peak);
                peak = file_bytes;
                if last.elapsed().as_millis() < 150 {
                    return;
                }
                last = std::time::Instant::now();
                let live = (base + file_bytes).min(total_bytes);
                let rate = (live as f64 / start.elapsed().as_secs_f64().max(0.001)) as u64;
                up.set(downloading(i as u64, total, live, total_bytes, rate, &remote));
            }
        };
        let mut res = net::download_to(&file_url, &dest, &sha, size, true, progress).await;
        if let Err(err) = &res {
            crate::log(&format!("update: [{}/{}] {} interrupted ({err}); retrying fresh", i + 1, total, e.path));
            // Retry fresh — keep showing the finished-files baseline as it re-streams.
            let up2 = up.clone();
            let remote2 = remote.clone();
            let base = done_bytes;
            let mut last = std::time::Instant::now();
            let progress = move |file_bytes: u64| {
                if last.elapsed().as_millis() < 150 {
                    return;
                }
                last = std::time::Instant::now();
                let live = (base + file_bytes.min(size)).min(total_bytes);
                let rate = (live as f64 / start.elapsed().as_secs_f64().max(0.001)) as u64;
                up2.set(downloading(i as u64, total, live, total_bytes, rate, &remote2));
            };
            res = net::download_to(&file_url, &dest, &sha, size, false, progress).await;
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
    // Persist the pendrive marker BEFORE announcing Ready: if the apply is then
    // interrupted (crash, power loss, a busy-mount teardown), the next launch finds
    // the marker and applies the already-staged delta (apply::resume_if_interrupted)
    // without needing the in-memory Pending or the network.
    write_pending_marker(&remote.commit, &remote.version, &staging);
    *up.pending.lock().unwrap() = Some(Pending { staging, remote: remote.clone(), sel });
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

#[cfg(test)]
mod tests {
    use super::*;
    use plan_ai_manifest::Entry;

    fn entry(path: &str, sha: &str, plats: &[&str]) -> Entry {
        Entry {
            path: path.into(),
            kind: "file".into(),
            sha256: Some(sha.into()),
            size: Some(1),
            exec: false,
            platforms: plats.iter().map(|s| s.to_string()).collect(),
            target: None,
            feature: plan_ai_manifest::classify_feature(path),
        }
    }

    fn man(version: &str, commit: &str, files: Vec<Entry>) -> Manifest {
        Manifest {
            schema: 1,
            product: "p".into(),
            version: version.into(),
            commit: commit.into(),
            built_at: String::new(),
            update_url: String::new(),
            files,
        }
    }

    /// One test fn for every local_for_plan scenario: they all share the
    /// PLANAI_PORTABLE_ROOT env var (process-global), so separate #[test]s would
    /// race under the parallel test runner.
    #[test]
    fn local_for_plan_heals_readded_platform_and_full_refetches_across_versions() {
        let base = std::env::temp_dir().join(format!("planai-healtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("components/linux-x64")).unwrap();
        std::env::set_var("PLANAI_PORTABLE_ROOT", &base);

        let files = vec![
            entry("plan-ai.linux-x64.exe", "a", &["linux-x64"]),
            entry("plan-ai.exe", "b", &["win-x64"]),
            entry("components/linux-x64/ollama-linux-amd64.squashfs", "c", &["linux-x64"]),
        ];
        let local = man("1.0", "abc", files.clone());
        std::fs::write(base.join("update.json"), local.to_json_pretty()).unwrap();
        // linux artifacts present on disk; the win launcher is NOT (pruned earlier)
        std::fs::write(base.join("plan-ai.linux-x64.exe"), b"x").unwrap();
        std::fs::write(base.join("components/linux-x64/ollama-linux-amd64.squashfs"), b"x").unwrap();

        // 1) re-added platform, NO update available (same version+commit) → HEAL:
        //    the local manifest loses exactly the missing win entry, so the diff
        //    re-downloads it and nothing else.
        let remote = man("1.0", "abc", files.clone());
        let sel = Selection::new(vec!["linux-x64".into(), "win-x64".into()], vec![]);
        let healed = local_for_plan(&remote, &sel, &base).expect("heal keeps a local manifest");
        assert!(healed.file("plan-ai.exe").is_none(), "missing artifact dropped from local");
        assert!(healed.file("plan-ai.linux-x64.exe").is_some());
        let plan = manifest::diff(Some(&healed), &remote, &sel);
        let paths: Vec<_> = plan.to_download.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["plan-ai.exe"], "downloads exactly the re-added platform's file");
        assert!(plan.to_delete.is_empty());

        // 2) nothing missing for the selection → plain local manifest, empty plan.
        let sel_linux = Selection::new(vec!["linux-x64".into()], vec![]);
        let l = local_for_plan(&remote, &sel_linux, &base).expect("local manifest");
        let plan = manifest::diff(Some(&l), &remote, &sel_linux);
        assert!(plan.to_download.is_empty());

        // 3) missing artifact AND a new remote version → None (full re-fetch).
        let newer = man("2.0", "def", files);
        assert!(local_for_plan(&newer, &sel, &base).is_none());

        std::env::remove_var("PLANAI_PORTABLE_ROOT");
        let _ = std::fs::remove_dir_all(&base);
    }
}
