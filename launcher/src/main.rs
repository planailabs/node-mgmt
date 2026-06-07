// plan.ai native launcher — prepares the runtime, then runs Electron.
//
// Like the AppImage runtime, this rust binary is responsible for making the
// shipped components available to the app, then supervising it:
//   linux : MOUNT each component .squashfs via an embedded static squashfuse_ll
//           (extract via embedded unsquashfs if FUSE is unavailable)
//   macOS : mount each .dmg via `hdiutil attach`
//   windows: components ship pre-extracted as directories — used in place
// It assembles <cache>/dist/{runtime,ollama,ow-assets}, exports PLANAI_RESOURCES,
// launches the bundled Electron app, waits, and tears the mounts down on exit.
// (The Electron-side loader becomes a no-op when PLANAI_RESOURCES is already set.)
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// Embedded static tools (non-empty only on linux; see build.rs).
const SQUASHFUSE_LL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/squashfuse_ll"));
const UNSQUASHFS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/unsquashfs"));

struct Mount {
    dest: PathBuf,
    kind: MountKind,
}
enum MountKind {
    Fuse,
    Dmg,
    None,
}

fn log(msg: &str) {
    eprintln!("[plan-ai] {msg}");
}

/// Roots beside the launcher where the shared components/ pool lives.
fn external_roots(here: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(ai) = std::env::var_os("APPIMAGE") {
        if let Some(d) = Path::new(&ai).parent() {
            roots.push(d.to_path_buf());
        }
    }
    #[cfg(target_os = "macos")]
    {
        // .../plan.ai.app/Contents/MacOS/<exe> -> the dir containing the .app
        if let Some(p) = here.ancestors().nth(4) {
            roots.push(p.to_path_buf());
        }
    }
    roots.push(here.to_path_buf());
    if let Some(p) = here.parent() {
        roots.push(p.to_path_buf());
    }
    roots
}

fn components_dir(here: &Path) -> Option<PathBuf> {
    if let Some(c) = std::env::var_os("PLANAI_COMPONENTS") {
        let c = PathBuf::from(c);
        if c.join("manifest.json").exists() {
            return Some(c);
        }
    }
    for r in external_roots(here) {
        let c = r.join("components");
        if c.join("manifest.json").exists() {
            return Some(c);
        }
    }
    None
}

fn cache_root() -> PathBuf {
    if let Some(c) = std::env::var_os("PLANAI_CACHE") {
        return PathBuf::from(c);
    }
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("plan-ai")
}

fn lib_present(names: &[&str]) -> bool {
    let dirs = [
        "/opt/rocm/lib",
        "/usr/lib",
        "/usr/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/lib/x86_64-linux-gnu",
    ];
    names.iter().any(|n| dirs.iter().any(|d| Path::new(d).join(n).exists()))
}

/// Pick the ollama flavour base name present in `comp` for this machine.
fn detect_ollama(comp: &Path) -> Option<(String, String)> {
    let has = |key: &str| {
        ["squashfs", "tar.gz", "dmg"]
            .iter()
            .any(|e| comp.join(format!("ollama-{key}.{e}")).exists())
            || comp.join(format!("ollama-{key}")).is_dir()
    };
    let pick = |key: &str, why: &str| Some((format!("ollama-{key}"), why.to_string()));
    if cfg!(target_os = "linux") && std::env::consts::ARCH == "x86_64" {
        let rocm_ok = has("linux-amd64-rocm")
            && Path::new("/dev/kfd").exists()
            && lib_present(&["libamdhip64.so", "libamdhip64.so.6"]);
        if rocm_ok && std::env::var("PLANAI_OLLAMA").as_deref() != Ok("cpu") {
            return pick("linux-amd64-rocm", "AMD GPU + ROCm runtime detected");
        }
        if has("linux-amd64") {
            return pick("linux-amd64", "CPU (default)");
        }
    } else if cfg!(target_os = "linux") {
        if has("linux-arm64") {
            return pick("linux-arm64", "arm64 CPU");
        }
    } else if cfg!(target_os = "macos") {
        if has("darwin") {
            return pick("darwin", "macOS universal (Metal)");
        }
    } else if cfg!(target_os = "windows") {
        if has("windows-amd64") {
            return pick("windows-amd64", "Windows x64");
        }
    }
    None
}

// The shared pool may hold every platform's runtime; pick the one in THIS OS's
// format (linux=squashfs, mac=dmg, windows=pre-extracted dir).
fn runtime_base(comp: &Path) -> Option<String> {
    for ent in fs::read_dir(comp).ok()?.flatten() {
        let n = ent.file_name().to_string_lossy().into_owned();
        if !n.starts_with("runtime-") {
            continue;
        }
        #[cfg(target_os = "windows")]
        if ent.path().is_dir() {
            return Some(n);
        }
        #[cfg(target_os = "macos")]
        if n.ends_with(".dmg") {
            return Some(n.trim_end_matches(".dmg").to_string());
        }
        #[cfg(target_os = "linux")]
        if n.ends_with(".squashfs") {
            return Some(n.trim_end_matches(".squashfs").to_string());
        }
    }
    None
}

/// Write an embedded tool to `dir` once and return its path (linux only).
fn ensure_tool(dir: &Path, name: &str, bytes: &[u8]) -> Option<PathBuf> {
    if bytes.is_empty() {
        return None;
    }
    let p = dir.join(name);
    if !p.exists() {
        fs::create_dir_all(dir).ok()?;
        fs::write(&p, bytes).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o755));
        }
    }
    Some(p)
}

fn is_mountpoint(p: &Path) -> bool {
    match (fs::metadata(p), p.parent().and_then(|pp| fs::metadata(pp).ok())) {
        (Ok(a), Some(b)) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                return a.dev() != b.dev();
            }
            #[allow(unreachable_code)]
            false
        }
        _ => false,
    }
}

fn find_fusermount() -> Option<String> {
    if let Ok(p) = std::env::var("FUSERMOUNT_PROG") {
        return Some(p);
    }
    let mut dirs: Vec<String> = std::env::var("PATH").unwrap_or_default().split(':').map(String::from).collect();
    dirs.extend(["/run/wrappers/bin", "/usr/bin", "/bin", "/usr/local/bin"].map(String::from));
    for name in ["fusermount3", "fusermount"] {
        for d in &dirs {
            if !d.is_empty() && Path::new(d).join(name).exists() {
                return Some(format!("{d}/{name}"));
            }
        }
    }
    None
}

/// Make a component available at `dest`. Returns how it was provided (for teardown).
fn provide(comp: &Path, base: &str, dest: &Path, tools_dir: &Path, force_extract: bool) -> std::io::Result<MountKind> {
    // pre-extracted directory: windows' format (used in place). Only on windows —
    // on linux/mac a dir in the multi-platform pool belongs to windows; prefer the
    // OS's own mountable squashfs/dmg below.
    #[cfg(target_os = "windows")]
    {
        let dir = comp.join(base);
        if dir.is_dir() {
            let _ = fs::remove_dir_all(dest);
            std::os::windows::fs::symlink_dir(&dir, dest).or_else(|_| copy_dir(&dir, dest))?;
            log(&format!("{base}: directory used in place"));
            return Ok(MountKind::None);
        }
    }
    let squashfs = comp.join(format!("{base}.squashfs"));
    if squashfs.exists() {
        fs::create_dir_all(dest)?;
        if !force_extract {
            if let Some(sf) = ensure_tool(tools_dir, "squashfuse_ll", SQUASHFUSE_LL) {
                let mut cmd = Command::new(&sf);
                cmd.arg(&squashfs).arg(dest);
                if let Some(fm) = find_fusermount() {
                    cmd.env("FUSERMOUNT_PROG", fm);
                }
                if cmd.status().map(|s| s.success()).unwrap_or(false) && is_mountpoint(dest) {
                    log(&format!("{base}: mounted (squashfs)"));
                    return Ok(MountKind::Fuse);
                }
            }
        }
        // fallback: extract with unsquashfs
        if let Some(us) = ensure_tool(tools_dir, "unsquashfs", UNSQUASHFS) {
            log(&format!("{base}: extracting (squashfs)"));
            let ok = Command::new(&us)
                .args(["-f", "-no-progress", "-d"])
                .arg(dest)
                .arg(&squashfs)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                return Ok(MountKind::None);
            }
        }
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "mount/extract squashfs failed"));
    }
    let dmg = comp.join(format!("{base}.dmg"));
    if dmg.exists() {
        fs::create_dir_all(dest)?;
        let ok = Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-noverify", "-mountpoint"])
            .arg(dest)
            .arg(&dmg)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            log(&format!("{base}: mounted (dmg)"));
            return Ok(MountKind::Dmg);
        }
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "hdiutil attach failed"));
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("component {base} not found")))
}

#[allow(dead_code)] // used only on windows (dir-in-place fallback)
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for ent in fs::read_dir(src)? {
        let ent = ent?;
        let to = dst.join(ent.file_name());
        if ent.file_type()?.is_dir() {
            copy_dir(&ent.path(), &to)?;
        } else {
            fs::copy(ent.path(), &to)?;
        }
    }
    Ok(())
}

fn teardown(mounts: &[Mount]) {
    for m in mounts {
        match m.kind {
            MountKind::Fuse => {
                let fm = find_fusermount().unwrap_or_else(|| "fusermount".into());
                let _ = Command::new(fm).arg("-u").arg(&m.dest).status();
            }
            MountKind::Dmg => {
                let _ = Command::new("hdiutil").arg("detach").arg(&m.dest).status();
            }
            MountKind::None => {}
        }
    }
}

/// Locate the bundled Electron executable beside the launcher.
fn electron_target(here: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PLANAI_ELECTRON") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    for root in external_roots(here) {
        let mac = root.join("plan.ai.app/Contents/MacOS/plan.ai");
        if mac.exists() {
            return Some(mac);
        }
    }
    let me = std::env::current_exe().ok();
    for root in external_roots(here) {
        for name in ["plan.ai.exe", "plan-ai-usb", "plan.ai", "plan-ai"] {
            let p = root.join(name);
            if p.exists() && me.as_ref().map(|e| *e != p).unwrap_or(true) {
                return Some(p);
            }
        }
    }
    None
}

fn main() {
    let exe = std::env::current_exe().expect("current_exe");
    let here = exe.parent().expect("exe parent").to_path_buf();

    let mut mounts: Vec<Mount> = Vec::new();
    // If something upstream already prepared the resources, don't touch them.
    let mut resources = std::env::var_os("PLANAI_RESOURCES").map(PathBuf::from);

    if resources.is_none() {
        if let Some(comp) = components_dir(&here) {
            let root = cache_root().join("root");
            let dist = root.join("dist");
            let tools = root.join("tools");
            let _ = fs::create_dir_all(&dist);
            // On NixOS the generic ollama needs patchelf (writable) → extract.
            let force_extract = std::env::var_os("PLANAI_NIX_LD").is_some();

            let rt = runtime_base(&comp).unwrap_or_else(|| {
                log("no runtime component found");
                std::process::exit(1);
            });
            match provide(&comp, &rt, &dist.join("runtime"), &tools, force_extract) {
                Ok(k) => mounts.push(Mount { dest: dist.join("runtime"), kind: k }),
                Err(e) => { log(&format!("runtime: {e}")); std::process::exit(1); }
            }
            if comp.join("ow-assets.squashfs").exists() || comp.join("ow-assets.dmg").exists() || comp.join("ow-assets").is_dir() {
                if let Ok(k) = provide(&comp, "ow-assets", &dist.join("ow-assets"), &tools, force_extract) {
                    mounts.push(Mount { dest: dist.join("ow-assets"), kind: k });
                }
            }
            if let Some((ol, why)) = detect_ollama(&comp) {
                log(&format!("ollama flavour: {ol} — {why}"));
                match provide(&comp, &ol, &dist.join("ollama"), &tools, force_extract) {
                    Ok(k) => mounts.push(Mount { dest: dist.join("ollama"), kind: k }),
                    Err(e) => log(&format!("ollama: {e}")),
                }
                std::env::set_var("PLANAI_OLLAMA_FLAVOUR", ol);
                std::env::set_var("PLANAI_OLLAMA_REASON", why);
            }
            std::env::set_var("PLANAI_RESOURCES", &dist);
            resources = Some(dist);
        }
    }
    let _ = resources;

    let program = match electron_target(&here) {
        Some(p) => p,
        None => {
            log(&format!("could not find the bundled app beside {}", here.display()));
            teardown(&mounts);
            std::process::exit(1);
        }
    };

    // Supervise Electron (so we can tear the mounts down on exit), forwarding the
    // environment and user args.
    let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    let _ = &mut env;
    let mut cmd = Command::new(&program);
    #[cfg(target_os = "linux")]
    cmd.arg("--no-sandbox"); // read-only AppImage mount can't setuid chrome-sandbox
    cmd.args(std::env::args_os().skip(1));

    let code = match cmd.status() {
        Ok(st) => st.code().unwrap_or(0),
        Err(e) => {
            log(&format!("failed to start {}: {e}", program.display()));
            1
        }
    };
    teardown(&mounts);
    std::process::exit(code);
}
