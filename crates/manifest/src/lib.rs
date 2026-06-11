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

/// The platform this binary runs on, in manifest terms: an OS+arch *target* key
/// (`linux-x64` / `linux-arm64` / `win-x64` / `mac-arm64`). Component groups, kept
/// platforms, and classification are all keyed by this so two arches of one OS
/// (linux-x64 + linux-arm64) never collide in a shared OS bucket.
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win-x64"
    } else if cfg!(target_os = "macos") {
        "mac-arm64"
    } else if cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else {
        "linux-x64"
    }
}

/// The build targets this manifest scheme knows, as drive-path group segments.
pub const KNOWN_TARGET_KEYS: &[&str] = &["linux-x64", "linux-arm64", "win-x64", "mac-arm64"];

/// The optional-feature catalog: `(name, enabled_by_default)`. A manifest entry
/// tagged with a feature is only wanted when that feature is enabled in
/// platforms.json; entries without a feature are core (always wanted). Features
/// NOT enabled by default are never downloaded until the user turns them on.
/// `mgmt` has no components of its own — it toggles BEHAVIOR (the SPA's Config
/// tab + the usb daemon's networked parts, formerly the build-time `future` flag).
pub const KNOWN_FEATURES: &[(&str, bool)] = &[("openwebui", true), ("hermes", false), ("mgmt", false)];

/// The feature set a drive starts with (every default-on feature).
pub fn default_features() -> Vec<String> {
    KNOWN_FEATURES.iter().filter(|(_, on)| *on).map(|(n, _)| n.to_string()).collect()
}

/// Which optional feature owns a drive-relative path (None = core). Matched on the
/// component basename so it works for grouped (components/<target>/x) and flat
/// layouts, and for every packaging of one component (.squashfs/.dmg/.zip/dir).
pub fn classify_feature(rel: &str) -> Option<String> {
    let p = rel.to_ascii_lowercase();
    let base = p.rsplit('/').next().unwrap_or(p.as_str());
    if base.starts_with("hermes") {
        return Some("hermes".into());
    }
    // The python runtime exists to run Open-WebUI, and ow-assets is its model/asset
    // cache — together they ARE the openwebui feature (on by default).
    if base.starts_with("runtime-") || base.starts_with("ow-assets") {
        return Some("openwebui".into());
    }
    None
}

/// A manifest entry: a file, a directory, or a zip-component in the drive layout,
/// tagged with the platform(s) that need it.
///
/// A `zip` entry is a single archive that the launcher downloads and unpacks on
/// update — used for Windows components (otherwise thousands of individual files).
/// The archive's content lives at `path` (`…/foo.zip`) on the update server, but on
/// the burned USB image the zip is already UNPACKED into `target` and removed (the
/// manifest still carries the zip entry, identified by the archive's sha, so a later
/// update can diff against it). `target` is wiped before each unpack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String, // "file" | "dir" | "zip"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default)]
    pub exec: bool,
    /// "linux" | "mac" | "win" | "all" (all = every platform needs it).
    #[serde(default)]
    pub platforms: Vec<String>,
    /// For a `zip` entry: the drive-relative folder it unpacks into (wiped first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Optional feature this entry belongs to (None = core, always wanted). Tagged
    /// entries are only downloaded/kept when the feature is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
    /// A zip-component (downloaded as one archive, unpacked into `target`).
    pub fn is_zip(&self) -> bool {
        self.kind == "zip"
    }
    /// Does this drive's selection need the entry? A kept platform must match
    /// ("all" matches everything) AND, when the entry belongs to an optional
    /// feature, that feature must be enabled.
    pub fn wanted_by(&self, sel: &Selection) -> bool {
        let plat = self.platforms.iter().any(|p| p == "all")
            || self.platforms.iter().any(|p| sel.platforms.iter().any(|k| k == p));
        let feat = self.feature.as_ref().is_none_or(|f| sel.features.iter().any(|x| x == f));
        plat && feat
    }
}

/// What a drive keeps (platforms.json): the platform targets to retain + the
/// enabled optional features. A platforms.json without a `features` key (older
/// drives) gets the default feature set, preserving its pre-features behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    pub platforms: Vec<String>,
    #[serde(default = "default_features")]
    pub features: Vec<String>,
}

impl Selection {
    pub fn new(platforms: Vec<String>, features: Vec<String>) -> Self {
        Selection { platforms, features }
    }
    /// The bootstrap selection for a fresh drive: just this platform + defaults.
    pub fn current_platform_default() -> Self {
        Selection { platforms: vec![current_platform().to_string()], features: default_features() }
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
    // Top-level standalone launchers, one per target (linux ships both arches, so
    // the linux names carry the arch; win/mac have a single arch each).
    match p.as_str() {
        "plan-ai.linux-x64.exe" => return vec!["linux-x64".into()],
        "plan-ai.linux-arm64.exe" => return vec!["linux-arm64".into()],
        "plan-ai.exe" => return vec!["win-x64".into()],
        "plan-ai.dmg" => return vec!["mac-arm64".into()],
        _ => {}
    }
    // Per-target component group: everything under components/<target>/ (incl. that
    // group's manifest.json) belongs to exactly that target. Authoritative — the
    // grouping makes the layout self-describing, so we don't fall back to the
    // substring heuristics below for grouped paths.
    if let Some(rest) = p.strip_prefix("components/") {
        if let Some((seg, _)) = rest.split_once('/') {
            if KNOWN_TARGET_KEYS.contains(&seg) {
                return vec![seg.to_string()];
            }
        }
    }
    if p.starts_with("tools/") {
        // Vestigial (the launcher embeds its mount tools); if ever shipped, the
        // static musl tools serve both linux arches.
        return vec!["linux-x64".into(), "linux-arm64".into()];
    }
    // Fallback substring heuristic for ungrouped / back-compat paths. Arch can't be
    // inferred from a bare OS substring, so a matched OS expands to both of its
    // known target arches (only linux has more than one).
    let mut v = Vec::new();
    if p.contains("linux") || p.contains("nixos") {
        v.push("linux-x64".to_string());
        v.push("linux-arm64".to_string());
    }
    if p.contains("darwin") || p.contains("-mac-") || p.ends_with(".dmg") {
        v.push("mac-arm64".to_string());
    }
    if p.contains("windows") || p.contains("-win-") {
        v.push("win-x64".to_string());
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
        let feature = classify_feature(&rel);
        if md.is_dir() {
            out.push(Entry { path: rel, kind: "dir".into(), sha256: None, size: None, exec: false, platforms, target: None, feature });
            walk(root, &path, out)?;
        } else {
            // A `.zip` is a zip-component: a single archive the launcher unpacks into
            // a target folder (path minus the `.zip`). Used for Windows components.
            let (kind, target) = match rel.strip_suffix(".zip") {
                Some(stem) => ("zip", Some(stem.to_string())),
                None => ("file", None),
            };
            out.push(Entry {
                path: rel.clone(),
                kind: kind.into(),
                sha256: Some(sha256_file(&path)?),
                size: Some(md.len()),
                exec: is_exec(&md),
                platforms,
                target,
                feature,
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
    /// Target folders of zip-components that are no longer wanted (a pruned platform):
    /// the launcher wipes each (its unpacked files aren't individually tracked).
    pub to_wipe_dirs: Vec<String>,
    pub total_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, sha: &str, plats: &[&str]) -> Entry {
        Entry {
            path: path.into(),
            kind: "file".into(),
            sha256: Some(sha.into()),
            size: Some(1),
            exec: false,
            platforms: plats.iter().map(|s| s.to_string()).collect(),
            target: None,
            feature: classify_feature(path),
        }
    }

    fn zip(path: &str, sha: &str, plats: &[&str]) -> Entry {
        Entry {
            path: path.into(),
            kind: "zip".into(),
            sha256: Some(sha.into()),
            size: Some(1),
            exec: false,
            platforms: plats.iter().map(|s| s.to_string()).collect(),
            target: Some(path.strip_suffix(".zip").unwrap().to_string()),
            feature: classify_feature(path),
        }
    }

    fn sel(plats: &[&str]) -> Selection {
        Selection::new(plats.iter().map(|s| s.to_string()).collect(), default_features())
    }

    fn sel_feat(plats: &[&str], feats: &[&str]) -> Selection {
        Selection::new(
            plats.iter().map(|s| s.to_string()).collect(),
            feats.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn classify_by_name() {
        assert_eq!(classify("plan-ai.exe"), vec!["win-x64"]);
        assert_eq!(classify("plan-ai.dmg"), vec!["mac-arm64"]);
        assert_eq!(classify("plan-ai.linux-x64.exe"), vec!["linux-x64"]);
        assert_eq!(classify("plan-ai.linux-arm64.exe"), vec!["linux-arm64"]);
        assert_eq!(classify("tools/squashfuse_ll"), vec!["linux-x64", "linux-arm64"]);
        // ungrouped substring fallback: a bare OS expands to both of its arches
        assert_eq!(classify("components/nixos-fhs.closure"), vec!["linux-x64", "linux-arm64"]);
        assert_eq!(classify("components/ow-assets.squashfs"), vec!["all"]);
    }

    #[test]
    fn classify_per_target_component_group() {
        // Everything under components/<target>/ is tagged to that target by the group
        // segment — incl. files the substring heuristic would miss (ow-assets, the
        // group's manifest.json) or mis-tag. Two linux arches stay distinct.
        assert_eq!(classify("components/linux-x64/runtime-linux-x64.squashfs"), vec!["linux-x64"]);
        assert_eq!(classify("components/linux-arm64/runtime-linux-arm64.squashfs"), vec!["linux-arm64"]);
        assert_eq!(classify("components/linux-arm64/nixos-fhs.closure"), vec!["linux-arm64"]);
        assert_eq!(classify("components/linux-x64/ow-assets.squashfs"), vec!["linux-x64"]);
        assert_eq!(classify("components/linux-arm64/manifest.json"), vec!["linux-arm64"]);
        assert_eq!(classify("components/win-x64/app-win-x64"), vec!["win-x64"]);
        assert_eq!(classify("components/win-x64/manifest.json"), vec!["win-x64"]);
        assert_eq!(classify("components/mac-arm64/ow-assets.dmg"), vec!["mac-arm64"]);
        assert_eq!(classify("components/mac-arm64/manifest.json"), vec!["mac-arm64"]);
    }

    #[test]
    fn safety_and_exclusion() {
        assert!(!is_safe_path("models/x"));
        assert!(!is_safe_path("../escape"));
        assert!(!is_safe_path("/abs"));
        assert!(is_safe_path("components/x"));
        assert!(is_excluded("data/foo"));
        assert!(is_excluded("update.json"));
        assert!(!is_excluded("components/x"));
    }

    fn manifest(files: Vec<Entry>) -> Manifest {
        Manifest { schema: 1, product: "p".into(), version: "1".into(), commit: "c".into(), built_at: String::new(), update_url: String::new(), files }
    }

    #[test]
    fn diff_bootstrap_keeps_only_wanted_platforms() {
        let remote = manifest(vec![
            file("plan-ai.linux-x64.exe", "a", &["linux-x64"]),
            file("plan-ai.exe", "b", &["win-x64"]),
            file("components/ow-assets.squashfs", "c", &["all"]),
        ]);
        let plan = diff(None, &remote, &sel(&["linux-x64"]));
        let paths: Vec<_> = plan.to_download.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"plan-ai.linux-x64.exe"));
        assert!(paths.contains(&"components/ow-assets.squashfs")); // "all" wanted
        assert!(!paths.contains(&"plan-ai.exe")); // win not kept
        assert!(plan.to_delete.is_empty());
    }

    #[test]
    fn diff_prunes_other_platforms_and_skips_unchanged() {
        let local = manifest(vec![
            file("plan-ai.linux-x64.exe", "a", &["linux-x64"]),
            file("plan-ai.exe", "b", &["win-x64"]), // present from a prior multi-platform build
        ]);
        let remote = manifest(vec![
            file("plan-ai.linux-x64.exe", "a", &["linux-x64"]), // unchanged
            file("plan-ai.exe", "b", &["win-x64"]),
        ]);
        let plan = diff(Some(&local), &remote, &sel(&["linux-x64"]));
        assert!(plan.to_download.is_empty()); // linux unchanged, win not wanted
        assert_eq!(plan.to_delete, vec!["plan-ai.exe".to_string()]); // prune win
    }

    #[test]
    fn diff_redownloads_changed() {
        let local = manifest(vec![file("plan-ai.linux-x64.exe", "old", &["linux-x64"])]);
        let remote = manifest(vec![file("plan-ai.linux-x64.exe", "new", &["linux-x64"])]);
        let plan = diff(Some(&local), &remote, &sel(&["linux-x64"]));
        assert_eq!(plan.to_download.len(), 1);
    }

    #[test]
    fn zip_entry_downloads_when_changed_and_carries_target() {
        let local = manifest(vec![zip("components/win-x64/runtime-win-x64.zip", "old", &["win-x64"])]);
        let remote = manifest(vec![zip("components/win-x64/runtime-win-x64.zip", "new", &["win-x64"])]);
        let plan = diff(Some(&local), &remote, &sel(&["win-x64"]));
        assert_eq!(plan.to_download.len(), 1);
        let e = &plan.to_download[0];
        assert!(e.is_zip());
        assert_eq!(e.target.as_deref(), Some("components/win-x64/runtime-win-x64"));
        assert!(plan.to_wipe_dirs.is_empty()); // a changed zip wipes its target on apply, not via to_wipe_dirs
    }

    #[test]
    fn zip_entry_unchanged_is_skipped() {
        let local = manifest(vec![zip("components/win-x64/app-win-x64.zip", "same", &["win-x64"])]);
        let remote = manifest(vec![zip("components/win-x64/app-win-x64.zip", "same", &["win-x64"])]);
        let plan = diff(Some(&local), &remote, &sel(&["win-x64"]));
        assert!(plan.to_download.is_empty());
        assert!(plan.to_wipe_dirs.is_empty());
    }

    #[test]
    fn pruned_zip_platform_wipes_target_not_delete() {
        // A win drive's update.json carries a win zip; on a linux-only machine it's
        // pruned by wiping its unpacked target folder (the .zip file isn't on disk,
        // and its unpacked files aren't individually tracked, so to_delete is wrong).
        let local = manifest(vec![
            file("plan-ai.linux-x64.exe", "a", &["linux-x64"]),
            zip("components/win-x64/runtime-win-x64.zip", "b", &["win-x64"]),
        ]);
        let remote = manifest(vec![
            file("plan-ai.linux-x64.exe", "a", &["linux-x64"]),
            zip("components/win-x64/runtime-win-x64.zip", "b", &["win-x64"]),
        ]);
        let plan = diff(Some(&local), &remote, &sel(&["linux-x64"]));
        assert!(plan.to_download.is_empty());
        assert!(plan.to_delete.is_empty());
        assert_eq!(plan.to_wipe_dirs, vec!["components/win-x64/runtime-win-x64".to_string()]);
    }

    #[test]
    fn classify_zip_component_by_group() {
        assert_eq!(classify("components/win-x64/runtime-win-x64.zip"), vec!["win-x64"]);
        assert_eq!(classify("components/win-x64/app-win-x64.zip"), vec!["win-x64"]);
    }

    #[test]
    fn classify_feature_by_basename() {
        assert_eq!(classify_feature("components/linux-x64/hermes-linux-x64.squashfs").as_deref(), Some("hermes"));
        assert_eq!(classify_feature("components/win-x64/hermes-win-x64.zip").as_deref(), Some("hermes"));
        assert_eq!(classify_feature("components/linux-x64/runtime-linux-x64.squashfs").as_deref(), Some("openwebui"));
        assert_eq!(classify_feature("components/mac-arm64/ow-assets.dmg").as_deref(), Some("openwebui"));
        // core stays untagged
        assert_eq!(classify_feature("components/linux-x64/ollama-linux-amd64.squashfs"), None);
        assert_eq!(classify_feature("components/linux-x64/manifest.json"), None);
        assert_eq!(classify_feature("plan-ai.exe"), None);
    }

    #[test]
    fn selection_defaults_features_when_key_missing() {
        // an older platforms.json (no `features` key) keeps the default behavior
        let s: Selection = serde_json::from_str(r#"{ "platforms": ["linux-x64"] }"#).unwrap();
        assert_eq!(s.features, vec!["openwebui".to_string()]);
        let s: Selection = serde_json::from_str(r#"{ "platforms": ["linux-x64"], "features": [] }"#).unwrap();
        assert!(s.features.is_empty());
    }

    #[test]
    fn default_off_feature_not_downloaded_by_default() {
        let remote = manifest(vec![
            file("plan-ai.linux-x64.exe", "a", &["linux-x64"]),
            file("components/linux-x64/runtime-linux-x64.squashfs", "b", &["linux-x64"]),
            file("components/linux-x64/hermes-linux-x64.squashfs", "c", &["linux-x64"]),
        ]);
        // bootstrap with the default feature set: hermes (default-off) is skipped
        let plan = diff(None, &remote, &sel(&["linux-x64"]));
        let paths: Vec<_> = plan.to_download.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"components/linux-x64/runtime-linux-x64.squashfs"));
        assert!(!paths.iter().any(|p| p.contains("hermes")));
        // enabling hermes pulls it in
        let plan = diff(None, &remote, &sel_feat(&["linux-x64"], &["openwebui", "hermes"]));
        let paths: Vec<_> = plan.to_download.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.iter().any(|p| p.contains("hermes")));
    }

    #[test]
    fn disabling_feature_prunes_its_components() {
        let local = manifest(vec![
            file("plan-ai.linux-x64.exe", "a", &["linux-x64"]),
            file("components/linux-x64/runtime-linux-x64.squashfs", "b", &["linux-x64"]),
            zip("components/win-x64/runtime-win-x64.zip", "c", &["win-x64"]),
            file("components/linux-x64/hermes-linux-x64.squashfs", "d", &["linux-x64"]),
        ]);
        let remote = local.clone();
        // every feature off: openwebui + hermes artifacts pruned, the zip via wipe
        let plan = diff(Some(&local), &remote, &sel_feat(&["linux-x64", "win-x64"], &[]));
        assert!(plan.to_download.is_empty());
        assert!(plan.to_delete.contains(&"components/linux-x64/runtime-linux-x64.squashfs".to_string()));
        assert!(plan.to_delete.contains(&"components/linux-x64/hermes-linux-x64.squashfs".to_string()));
        assert_eq!(plan.to_wipe_dirs, vec!["components/win-x64/runtime-win-x64".to_string()]);
        assert!(!plan.to_delete.contains(&"plan-ai.linux-x64.exe".to_string())); // core stays
    }

    #[test]
    fn reenabling_feature_redownloads_it() {
        // local manifest still lists the hermes entry (manifests are full),
        // but the diff with the feature re-enabled re-downloads only if changed —
        // the on-disk heal path (launcher update::local_for_plan) handles the
        // "file gone but sha unchanged" case; here we prove the wanted logic.
        let local = manifest(vec![file("components/linux-x64/hermes-linux-x64.squashfs", "d", &["linux-x64"])]);
        let remote = manifest(vec![file("components/linux-x64/hermes-linux-x64.squashfs", "e", &["linux-x64"])]);
        let plan = diff(Some(&local), &remote, &sel_feat(&["linux-x64"], &["hermes"]));
        assert_eq!(plan.to_download.len(), 1); // sha changed → re-download
    }
}

/// Diff `local` (what's on the drive; None ⇒ bootstrap) against `remote` for a
/// drive selection (kept platforms + enabled features). Wanted = entries the
/// selection needs; everything else local is pruned — including a disabled
/// feature's components, freeing their space. Never touches models/ or data/
/// (excluded from manifests).
pub fn diff(local: Option<&Manifest>, remote: &Manifest, sel: &Selection) -> Plan {
    let mut plan = Plan::default();
    for e in &remote.files {
        if !is_safe_path(&e.path) || !e.wanted_by(sel) {
            continue;
        }
        if e.is_dir() {
            plan.to_mkdir.push(e.path.clone());
            continue;
        }
        // Files AND zip-components download by sha (a changed zip → re-download the
        // whole archive; the launcher wipes + re-unpacks its target on apply).
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
            let remote_has = remote.file(&e.path).is_some_and(|r| r.wanted_by(sel));
            // Remove if the remote no longer ships it, or the selection no longer
            // wants it (pruning another platform's / a disabled feature's components
            // to free space).
            if !remote_has || !e.wanted_by(sel) {
                if e.is_zip() {
                    // The zip itself isn't on the drive (unpacked); wipe its target
                    // folder instead (its files aren't individually tracked).
                    if let Some(t) = &e.target {
                        plan.to_wipe_dirs.push(t.clone());
                    }
                } else {
                    plan.to_delete.push(e.path.clone());
                }
            }
        }
    }
    plan
}
