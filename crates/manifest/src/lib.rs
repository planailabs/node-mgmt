//! Shared update-manifest schema + logic, used by BOTH the build tool (xtask, which
//! generates manifests for the USB image + the update server) and the launcher
//! updater (which diffs a local manifest against the remote one and applies the
//! delta). One source of truth ⇒ the generator and the updater can never drift.

use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The default update server (overridable per-build via the manifest's `update_url`).
pub const DEFAULT_UPDATE_URL: &str = "https://usb-update.plan.ai";

/// The platform this binary runs on, in manifest terms.
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else {
        "linux"
    }
}

/// A manifest entry: a file or a directory in the drive layout, tagged with the
/// platform(s) that need it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String, // "file" | "dir"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default)]
    pub exec: bool,
    /// "linux" | "mac" | "win" | "all" (all = every platform needs it).
    #[serde(default)]
    pub platforms: Vec<String>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
    /// Does any kept platform need this entry? ("all" matches everything.)
    pub fn wanted_by(&self, kept: &[String]) -> bool {
        self.platforms.iter().any(|p| p == "all") || self.platforms.iter().any(|p| kept.iter().any(|k| k == p))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub product: String,
    pub version: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub built_at: String,
    #[serde(default)]
    pub update_url: String,
    pub files: Vec<Entry>,
}

impl Manifest {
    pub fn from_json(s: &str) -> serde_json::Result<Manifest> {
        serde_json::from_str(s)
    }
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
    pub fn file(&self, path: &str) -> Option<&Entry> {
        self.files.iter().find(|e| e.path == path && !e.is_dir())
    }
}

/// Which platform(s) need a drive-relative path. Shared, lowercase, substring
/// classification — deliberately avoids the bare "win"/"mac" tokens (they hide
/// inside "darwin"). Anything unmatched is shared ("all").
pub fn classify(rel: &str) -> Vec<String> {
    let p = rel.to_ascii_lowercase();
    match p.as_str() {
        "plan-ai.linux.exe" => return vec!["linux".into()],
        "plan-ai.exe" => return vec!["win".into()],
        "plan-ai.dmg" => return vec!["mac".into()],
        _ => {}
    }
    if p.starts_with("tools/") {
        return vec!["linux".into()];
    }
    let mut v = Vec::new();
    if p.contains("linux") || p.contains("nixos") {
        v.push("linux".to_string());
    }
    if p.contains("darwin") || p.contains("-mac-") || p.ends_with(".dmg") {
        v.push("mac".to_string());
    }
    if p.contains("windows") || p.contains("-win-") {
        v.push("win".to_string());
    }
    if v.is_empty() {
        v.push("all".to_string());
    }
    v
}

/// Reject paths that escape the drive root or touch user data — used by both the
/// generator (defensive) and the updater (security: never delete/overwrite these).
pub fn is_safe_path(rel: &str) -> bool {
    if rel.is_empty() || rel.starts_with('/') || rel.contains("..") || rel.contains('\\') {
        return false;
    }
    let first = rel.split('/').next().unwrap_or("");
    !matches!(first, "models" | "data")
}

/// Paths excluded from the managed set entirely (user data + manifest markers).
pub fn is_excluded(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or("");
    matches!(first, "models" | "data")
        || matches!(rel, "update.json" | "platforms.json" | ".update-applying.json")
}

/// Streamed sha256 of a file, lowercase hex.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Walk `root` (following symlinks) and build a manifest of the managed set.
pub fn generate(root: &Path, version: &str, commit: &str, update_url: &str, built_at: &str) -> io::Result<Manifest> {
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Manifest {
        schema: 1,
        product: "plan-ai-usb".into(),
        version: version.into(),
        commit: commit.into(),
        built_at: built_at.into(),
        update_url: update_url.into(),
        files,
    })
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> io::Result<()> {
    let mut ents: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    ents.sort_by_key(|e| e.file_name());
    for e in ents {
        let path = e.path();
        // metadata() follows symlinks (the USB mirror is symlinks).
        let md = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        if is_excluded(&rel) {
            continue;
        }
        let platforms = classify(&rel);
        if md.is_dir() {
            out.push(Entry { path: rel, kind: "dir".into(), sha256: None, size: None, exec: false, platforms });
            walk(root, &path, out)?;
        } else {
            out.push(Entry {
                path: rel.clone(),
                kind: "file".into(),
                sha256: Some(sha256_file(&path)?),
                size: Some(md.len()),
                exec: is_exec(&md),
                platforms,
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
fn is_exec(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_exec(_md: &std::fs::Metadata) -> bool {
    false
}

/// The work needed to bring a drive in line with the remote manifest for a set of
/// kept platforms: files to (re)download, files to delete (changed-away or pruned
/// platforms), dirs to create, and the total download size.
#[derive(Debug, Default)]
pub struct Plan {
    pub to_download: Vec<Entry>,
    pub to_delete: Vec<String>,
    pub to_mkdir: Vec<String>,
    pub total_bytes: u64,
}

/// Diff `local` (what's on the drive; None ⇒ bootstrap) against `remote` for the
/// `kept` platforms. Wanted = entries any kept platform needs; everything else
/// local is pruned. Never touches models/ or data/ (excluded from manifests).
pub fn diff(local: Option<&Manifest>, remote: &Manifest, kept: &[String]) -> Plan {
    let mut plan = Plan::default();
    for e in &remote.files {
        if !is_safe_path(&e.path) || !e.wanted_by(kept) {
            continue;
        }
        if e.is_dir() {
            plan.to_mkdir.push(e.path.clone());
            continue;
        }
        let unchanged = local.and_then(|l| l.file(&e.path)).is_some_and(|cur| cur.sha256 == e.sha256 && cur.size == e.size);
        if !unchanged {
            plan.total_bytes += e.size.unwrap_or(0);
            plan.to_download.push(e.clone());
        }
    }
    if let Some(local) = local {
        for e in &local.files {
            if e.is_dir() || !is_safe_path(&e.path) {
                continue;
            }
            let remote_has = remote.file(&e.path).is_some_and(|r| r.wanted_by(kept));
            // Delete if the remote no longer ships it, or no kept platform wants it
            // (pruning another platform's files to free space).
            if !remote_has || !e.wanted_by(kept) {
                plan.to_delete.push(e.path.clone());
            }
        }
    }
    plan
}
