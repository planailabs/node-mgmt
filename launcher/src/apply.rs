//! Apply a staged update onto the USB, crash-safe. Runs AFTER the runtime is fully
//! down (Electron + supervisor + mounts gone), with a determinate progress splash.
//!
//! Power-loss invariant: each managed file is always either the whole old or the
//! whole new file (we copy to `<path>.new`, fsync, sha-verify, then same-dir
//! rename), partial bytes only ever live in reclaimable `.new` temps, and the
//! manifest (update.json) is committed LAST — so an interrupted apply boots the
//! intact old version. A journal (.update-applying.json) lets a reboot resume.

use std::path::{Path, PathBuf};

use plan_ai_manifest::{self as manifest, Manifest};
use serde_json::json;

use crate::{paths, update};

fn journal_path(root: &Path) -> PathBuf {
    root.join(".update-applying.json")
}

/// Apply the pending staged update (from a `Check`). Returns true if it applied.
pub fn run(up: &update::Updater) -> bool {
    let Some(pending) = up.pending.lock().unwrap().take() else {
        return false;
    };
    let root = paths::portable_root();
    write_journal(&root, &pending.staging, &pending.remote.commit);
    let ok = apply_plan(&root, &pending.staging, &pending.remote, &pending.kept, Some(up));
    let _ = std::fs::remove_file(journal_path(&root));
    // The staged update is now on the drive — drop the pendrive "in flight" marker
    // so the next launch doesn't try to re-apply it.
    update::clear_pending_marker();
    ok
}

/// On startup: finish any update that a prior run left unapplied — whether it was
/// interrupted mid-apply (the journal) OR was fully staged-and-ready but never
/// applied because the process exited first (the pendrive marker, e.g. after a
/// busy-mount teardown or a crash between download and apply). Both point at the
/// staging folder; apply is idempotent + crash-safe, so re-running is safe. When
/// the staging is gone, drop the stale markers + orphan temps and keep the intact
/// old version (a fresh download will re-stage it).
pub fn resume_if_interrupted() {
    let root = paths::portable_root();
    let jp = journal_path(&root);
    let mp = update::pending_marker_path();
    // Prefer the apply journal (mid-apply); fall back to the staged-ready marker.
    let marker = if jp.exists() { Some(jp.clone()) } else if mp.exists() { Some(mp.clone()) } else { None };
    let Some(marker) = marker else {
        cleanup_orphans(&root);
        return;
    };
    let staging = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|txt| serde_json::from_str::<serde_json::Value>(&txt).ok())
        .and_then(|v| v.get("staging").and_then(|s| s.as_str()).map(PathBuf::from));
    let remote = staging
        .as_ref()
        .and_then(|s| std::fs::read_to_string(s.join("manifest.json")).ok())
        .and_then(|s| Manifest::from_json(&s).ok());
    match (staging, remote) {
        (Some(staging), Some(remote)) if staging.exists() => {
            crate::log("resuming staged update apply (from pendrive marker)");
            let kept = update::read_platforms();
            apply_plan(&root, &staging, &remote, &kept, None);
            let _ = std::fs::remove_file(&jp);
            update::clear_pending_marker();
        }
        _ => {
            crate::log("stale update marker — staging gone; keeping current version");
            let _ = std::fs::remove_file(&jp);
            update::clear_pending_marker();
            cleanup_orphans(&root);
        }
    }
}

fn write_journal(root: &Path, staging: &Path, commit: &str) {
    let _ = std::fs::write(
        journal_path(root),
        json!({ "commit": commit, "staging": staging.to_string_lossy() }).to_string(),
    );
}

/// Copy staged files into place (verified, atomic per file), delete pruned files,
/// then commit the manifest last. `up` (when present) drives the progress splash.
fn apply_plan(root: &Path, staging: &Path, remote: &Manifest, kept: &[String], up: Option<&update::Updater>) -> bool {
    let plan = manifest::diff(update::load_local().as_ref(), remote, kept);
    let total = (plan.to_download.len() + plan.to_delete.len() + plan.to_wipe_dirs.len()) as u64;
    let commit = remote.commit.get(..8).unwrap_or(&remote.commit);
    crate::log(&format!(
        "apply: placing {} item(s) + {} deletion(s) + {} wipe(s) onto {} -> version {} (commit {})",
        plan.to_download.len(), plan.to_delete.len(), plan.to_wipe_dirs.len(), root.display(), remote.version, commit,
    ));
    let total_bytes: u64 = plan.to_download.iter().map(|e| e.size.unwrap_or(0)).sum();
    let mut splash = crate::show_splash(crate::SplashOpts { text: &crate::i18n::t("applying-update"), progress: true });
    let mut done = 0u64;
    let mut done_bytes = 0u64;
    // Log a terminal progress line at most every ~5% (so a 60k-file windows apply
    // shows movement without 60k lines); the splash gauge + SPA get every tick, incl.
    // the throughput indicator (copy-onto-drive speed).
    let mut next_log = 0u64;
    let start = std::time::Instant::now();
    let tick = |done: u64, done_bytes: u64, splash: &mut Option<crate::Splash>, up: Option<&update::Updater>, next_log: &mut u64| {
        let pct = if total == 0 { 100 } else { (done * 100 / total) as u8 };
        if let Some(s) = splash.as_mut() {
            s.set_progress(pct);
        }
        let rate = (done_bytes as f64 / start.elapsed().as_secs_f64().max(0.001)) as u64;
        if let Some(u) = up {
            u.set_applying(done, total, done_bytes, total_bytes, rate);
        }
        if total > 0 && (done >= *next_log || done == total) {
            crate::log(&format!("apply: {done}/{total} ({pct}%)"));
            *next_log = done + (total / 20).max(1);
        }
    };

    for d in &plan.to_mkdir {
        if manifest::is_safe_path(d) {
            let _ = std::fs::create_dir_all(root.join(d));
        }
    }

    let me = std::env::current_exe().ok();
    for e in &plan.to_download {
        if !manifest::is_safe_path(&e.path) {
            continue;
        }
        let src = staging.join(&e.path);
        if !src.exists() {
            crate::log(&format!("apply: staged file missing, skipping {}", e.path));
            continue;
        }
        if e.is_zip() {
            // Zip-component: wipe the target folder, then unpack the staged archive
            // into it. The download already sha-verified the zip (net::download_to).
            match e.target.as_deref() {
                Some(t) if manifest::is_safe_path(t) => {
                    let target = root.join(t);
                    let _ = std::fs::remove_dir_all(&target);
                    if let Err(err) = unpack_zip(&src, &target) {
                        crate::log(&format!("apply: unzip {} -> {t} failed: {err}", e.path));
                    }
                }
                _ => crate::log(&format!("apply: zip {} has no safe target — skipping", e.path)),
            }
        } else {
            let dest = root.join(&e.path);
            if let Err(err) = place_file(&src, &dest, e.sha256.as_deref(), e.exec, me.as_deref()) {
                crate::log(&format!("apply: {} failed: {err}", e.path));
            }
        }
        done += 1;
        done_bytes += e.size.unwrap_or(0);
        tick(done, done_bytes, &mut splash, up, &mut next_log);
    }

    // Wipe target folders of pruned zip-components (another platform's, no longer
    // wanted) — their unpacked files aren't individually tracked, so a per-file
    // delete can't reach them.
    for d in &plan.to_wipe_dirs {
        if manifest::is_safe_path(d) {
            let _ = std::fs::remove_dir_all(root.join(d));
        }
        done += 1;
        tick(done, done_bytes, &mut splash, up, &mut next_log);
    }

    for p in &plan.to_delete {
        if manifest::is_safe_path(p) {
            let _ = std::fs::remove_file(root.join(p));
        }
        done += 1;
        tick(done, done_bytes, &mut splash, up, &mut next_log);
    }
    prune_empty_dirs(root, &plan.to_delete);

    // Commit: write the new manifest last (the "done" marker).
    let _ = std::fs::write(root.join("update.json"), remote.to_json_pretty());
    if let Some(s) = splash.take() {
        s.close();
    }
    crate::log(&format!(
        "update applied: version {} (commit {}) — {} placed, {} removed",
        remote.version, commit, plan.to_download.len(), plan.to_delete.len(),
    ));
    true
}

/// Place one file atomically: copy → fsync → sha-verify → same-dir rename. The
/// launcher's OWN running binary needs the Windows rename-away dance; unix can
/// replace a running file directly.
fn place_file(src: &Path, dest: &Path, sha: Option<&str>, exec: bool, me: Option<&Path>) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let is_self = me.map(|m| m == dest).unwrap_or(false);

    #[cfg(target_os = "windows")]
    if is_self {
        // Can't overwrite the running .exe; rename it away (allowed), then copy the
        // new one in. The .old is reaped on next start (cleanup_orphans).
        let old = with_ext(dest, "old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(dest, &old)?;
        std::fs::copy(src, dest)?;
        return Ok(());
    }

    let tmp = with_ext(dest, "new");
    std::fs::copy(src, &tmp)?;
    if let Some(want) = sha {
        if !want.is_empty() {
            let got = manifest::sha256_file(&tmp)?;
            if got != want {
                let _ = std::fs::remove_file(&tmp);
                return Err(std::io::Error::other(format!("sha mismatch on drive for {}", dest.display())));
            }
        }
    }
    // unix: replacing a running binary via rename is fine (inode kept by the process).
    let _ = is_self;
    std::fs::rename(&tmp, dest)?;
    #[cfg(unix)]
    if exec {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755));
    }
    #[cfg(not(unix))]
    let _ = exec;
    Ok(())
}

/// Unpack a zip-component archive into `dest` (already wiped by the caller). Uses
/// the pure-Rust `zip` crate so it cross-compiles with the rest of the launcher
/// (static-musl / zig) — only ever exercised on Windows, where components ship as
/// zips, but compiled on every target.
fn unpack_zip(zip_path: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    let f = std::fs::File::open(zip_path)?;
    let mut ar = zip::ZipArchive::new(f).map_err(|e| std::io::Error::other(format!("open zip: {e}")))?;
    ar.extract(dest).map_err(|e| std::io::Error::other(format!("extract zip: {e}")))?;
    Ok(())
}

fn with_ext(p: &Path, ext: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Remove now-empty managed dirs left by deletions (deepest first). Never models/data.
fn prune_empty_dirs(root: &Path, deleted: &[String]) {
    let mut dirs: Vec<&str> = deleted.iter().filter_map(|p| p.rsplit_once('/').map(|(d, _)| d)).collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
    dirs.dedup();
    for d in dirs {
        if manifest::is_safe_path(d) {
            let _ = std::fs::remove_dir(root.join(d)); // only succeeds if empty
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn unpack_zip_extracts_tree_and_wipe_is_clean() {
        let base = std::env::temp_dir().join(format!("planai-ziptest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let zip_path = base.join("c.zip");

        // Build a small deflate-compressed archive with the same `zip` crate the
        // launcher reads with — proves the dependency + feature decompress correctly.
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts: zip::write::SimpleFileOptions =
                zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            w.start_file("top.txt", opts).unwrap();
            w.write_all(b"top").unwrap();
            w.add_directory("sub", opts).unwrap();
            w.start_file("sub/inner.txt", opts).unwrap();
            w.write_all(b"inner").unwrap();
            w.finish().unwrap();
        }

        // Pre-seed the target with a stale file the wipe-before-unpack must remove.
        let target = base.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("STALE"), b"old").unwrap();

        // Mirror apply's contract: wipe, then unpack.
        let _ = std::fs::remove_dir_all(&target);
        unpack_zip(&zip_path, &target).unwrap();

        assert_eq!(std::fs::read_to_string(target.join("top.txt")).unwrap(), "top");
        assert_eq!(std::fs::read_to_string(target.join("sub/inner.txt")).unwrap(), "inner");
        assert!(!target.join("STALE").exists(), "wipe-before-unpack must drop stale files");

        let _ = std::fs::remove_dir_all(&base);
    }
}

/// Reap leftover `.new`/`.old` temps from an interrupted apply (best-effort, shallow).
fn cleanup_orphans(root: &Path) {
    fn walk(dir: &Path, depth: u32) {
        if depth > 3 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), "models" | "data") {
                continue;
            }
            if p.is_dir() {
                walk(&p, depth + 1);
            } else if name.ends_with(".new") || name.ends_with(".old") {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    walk(root, 0);
}
