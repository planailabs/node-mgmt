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
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod config;
mod control;
mod paths;
mod proxy;
mod serve;

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

/// Best-effort cross-platform desktop notification (notify-rust: Linux D-Bus,
/// macOS, Windows toast).
fn notify(title: &str, body: &str) {
    let _ = notify_rust::Notification::new().summary(title).body(body).show();
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
        // here = .../plan.ai.app/Contents/MacOS  →  up 3 = the dir containing the
        // .app (where the shared components/ pool sits beside the launcher .app).
        if let Some(p) = here.ancestors().nth(3) {
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

/// NixOS has a bare nix-ld stub that can't run a generic glibc FHS binary, and
/// mounting the read-only squashfs leaves no room to patchelf the bundled tools.
/// So on NixOS we EXTRACT components (writable) instead of FUSE-mounting — the
/// same path the node loader takes when PLANAI_NIX_LD is set. Detected via the
/// markers NixOS always creates. (Always false off linux.)
fn is_nixos() -> bool {
    if std::env::var_os("PLANAI_NIX_LD").is_some() {
        return true;
    }
    cfg!(target_os = "linux")
        && (Path::new("/etc/NIXOS").exists() || Path::new("/run/current-system/sw").exists())
}

// On NixOS the generic glibc Electron/ollama can't run (bare nix-ld stub). We ship
// a buildFHSEnv wrapper's closure as a NAR (its /nix/store paths don't exist on the
// target); import it, then run OURSELF inside the wrapper as a CHILD (not exec) so
// the components we already FUSE-mounted on the host stay mounted — the sandbox
// sees them through the wrapper's recursive bind — and THIS process survives to
// unmount them after the sandboxed app exits. Returns the child's exit code, or
// None when the FHS path doesn't apply (non-NixOS / dev / already inside / setup
// failed → the caller runs the app bare on the host). Guarded by PLANAI_FHS_REEXEC.
#[cfg(target_os = "linux")]
fn maybe_run_in_fhs(comp: Option<&Path>) -> Option<i32> {
    // Skip in dev (nixpkgs Electron + staged dist/, no generic binaries to sandbox)
    // and once already inside; only the prod NixOS path needs the FHS.
    if std::env::var_os("PLANAI_FHS_REEXEC").is_some()
        || std::env::var_os("PLANAI_DEV").is_some()
        || !is_nixos()
    {
        return None;
    }
    let comp = comp?;
    let wrapper = fs::read_to_string(comp.join("nixos-fhs.path")).ok()?.trim().to_string();
    if wrapper.is_empty() {
        return None; // no FHS helper shipped — best-effort, run bare
    }
    if !Path::new(&wrapper).exists() {
        let closure = comp.join("nixos-fhs.closure");
        if !closure.exists() {
            log("NixOS FHS: helper closure missing — generic binaries may not run");
            return None;
        }
        log("NixOS FHS: importing helper closure into the nix store (first run)");
        let f = match fs::File::open(&closure) {
            Ok(f) => f,
            Err(e) => { log(&format!("NixOS FHS: open closure: {e}")); return None; }
        };
        let ok = Command::new("nix-store")
            .arg("--import")
            .stdin(f)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok || !Path::new(&wrapper).exists() {
            log("NixOS FHS: import failed (untrusted nix user? nix missing?) — running bare");
            return None;
        }
    }
    log("NixOS FHS: running launcher inside the FHS sandbox (components mounted on host)");
    let self_exe = std::env::current_exe().unwrap_or_default();
    match Command::new(&wrapper)
        .arg(&self_exe)
        .args(std::env::args_os().skip(1))
        .env("PLANAI_FHS_REEXEC", "1")
        .status()
    {
        Ok(s) => Some(s.code().unwrap_or(0)),
        Err(e) => { log(&format!("NixOS FHS: spawn {wrapper} failed: {e} — running bare")); None }
    }
}

/// Single-instance guard: take an exclusive advisory lock on a file in the cache
/// root. Returns the locked File (keep it alive for the whole run — the OS frees
/// the lock when the process exits) or None if another instance already holds it.
/// Best-effort: if the lock file can't be created we return Some(dummy)-equivalent
/// by proceeding (None only means "another instance is running").
fn acquire_instance_lock() -> Result<std::fs::File, bool> {
    use fs2::FileExt;
    let path = cache_root().join("instance.lock");
    if let Some(p) = path.parent() {
        let _ = fs::create_dir_all(p);
    }
    let file = match fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path) {
        Ok(f) => f,
        // Can't open the lock file (read-only FS etc.) — don't block startup.
        Err(e) => {
            log(&format!("instance lock: cannot open {} ({e}) — skipping guard", path.display()));
            return Err(false); // false = "couldn't lock, proceed anyway"
        }
    };
    match file.try_lock_exclusive() {
        Ok(()) => Ok(file),
        Err(_) => Err(true), // true = "another instance holds the lock"
    }
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

// The shared pool may hold every platform's copy of a component; pick the one
// whose name starts with `prefix` in THIS OS's format (linux=squashfs, mac=dmg,
// windows=pre-extracted dir). Used for the runtime AND the Electron app itself
// (both ship as components — app-<os>: app-linux-x64.squashfs / app-win-x64/ /
// app-mac-arm64.dmg).
fn pick_base(comp: &Path, prefix: &str) -> Option<String> {
    for ent in fs::read_dir(comp).ok()?.flatten() {
        let n = ent.file_name().to_string_lossy().into_owned();
        if !n.starts_with(prefix) {
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
            if let Some(parent) = dest.parent() { let _ = fs::create_dir_all(parent); }
            // Prefer a directory JUNCTION (mklink /J) — unlike a symlink it needs no
            // admin/developer-mode and no copy. Fall back to a symlink, then a copy.
            let junction = Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(dest)
                .arg(&dir)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if junction {
                log(&format!("{base}: directory junction (used in place)"));
                return Ok(MountKind::None);
            }
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
        // Compressed UDIF dmgs (built on Linux via libdmg-hfsplus) mount with a
        // normal hdiutil attach. Older bare HFS+ images (mkfs.hfsplus, no UDIF
        // wrapper) need the raw-disk-image class ("image not recognised"
        // otherwise) — try the normal path first, then fall back for back-compat.
        let attach = |extra: &[&str]| {
            Command::new("hdiutil")
                .args(["attach", "-nobrowse", "-noverify"])
                .args(extra)
                .arg("-mountpoint")
                .arg(dest)
                .arg(&dmg)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if attach(&[]) || attach(&["-imagekey", "diskimage-class=CRawDiskImage"]) {
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

/// Flush filesystem write buffers to disk on exit, so data the stack wrote to the
/// (USB) drive — Open-WebUI's DATA_DIR, ollama models under the portable root — is
/// persisted before the user pulls it, then tell the user it's safe to unplug.
/// Best-effort; call AFTER tearing the component mounts down for maximum safety.
fn flush_drive() {
    #[cfg(unix)]
    {
        // sync(1) flushes all mounted filesystems' buffers (incl. the USB).
        let _ = Command::new("sync").status();
    }
    #[cfg(windows)]
    {
        // Flush the volume that holds the portable root (models/ + data/).
        let root = paths::portable_root();
        if let Some(drive) = root.to_str().map(|s| s.trim_start_matches(r"\\?\")).and_then(|s| s.chars().next()) {
            let _ = Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command",
                       &format!("Write-VolumeCache -DriveLetter {drive}")])
                .status();
        }
    }
    notify("plan.ai", "Drive flushed — safe to unplug.");
}

fn teardown(mounts: &[Mount]) {
    for m in mounts {
        match m.kind {
            MountKind::Fuse => {
                let fm = find_fusermount().unwrap_or_else(|| "fusermount".into());
                let ok = Command::new(&fm).arg("-u").arg(&m.dest).status().map(|s| s.success()).unwrap_or(false);
                if !ok {
                    // A child may still be releasing the mount (mmap'd binary mid-exit).
                    // Lazy-detach: drops now, frees when the last reference closes —
                    // avoids leaking the squashfuse_ll mount on a teardown race.
                    let _ = Command::new(&fm).arg("-uz").arg(&m.dest).status();
                }
            }
            MountKind::Dmg => {
                let ok = Command::new("hdiutil").arg("detach").arg(&m.dest).status().map(|s| s.success()).unwrap_or(false);
                if !ok {
                    // Volume still busy (a child mid-exit) — force-detach (analog of
                    // the FUSE lazy unmount) so the dmg doesn't leak.
                    let _ = Command::new("hdiutil").arg("detach").arg("-force").arg(&m.dest).status();
                }
            }
            MountKind::None => {}
        }
    }
}

/// Find the Electron executable inside a provided app component tree (the dir the
/// app-<os> component was mounted/linked/extracted to):
///   macOS  : <app>/plan.ai.app/Contents/MacOS/plan.ai
///   windows: <app>/plan.ai.exe
///   linux  : <app>/plan-ai (electron-builder executableName)
fn electron_in(app: &Path) -> Option<PathBuf> {
    let mac = app.join("plan.ai.app/Contents/MacOS/plan.ai");
    if mac.exists() {
        return Some(mac);
    }
    for name in ["plan.ai.exe", "plan-ai", "plan.ai", "plan-ai-usb"] {
        let p = app.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Fallback: locate a bundled Electron executable beside the launcher (back-compat
/// with an app shipped next to the launcher rather than as an app-<os> component).
fn electron_target(here: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PLANAI_ELECTRON") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let me = std::env::current_exe().ok();
    for root in external_roots(here) {
        let mac = root.join("plan.ai.app/Contents/MacOS/plan.ai");
        if mac.exists() && me.as_ref().map(|e| *e != mac).unwrap_or(true) {
            return Some(mac);
        }
        for name in ["plan.ai.exe", "plan-ai-usb", "plan.ai", "plan-ai"] {
            let p = root.join(name);
            if p.exists() && me.as_ref().map(|e| *e != p).unwrap_or(true) {
                return Some(p);
            }
        }
    }
    None
}

/// The bundled llmfit binary for THIS OS, in the shared pool (OS-distinct names so
/// one pool can hold every platform's copy).
fn pool_llmfit(comp: &Path) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(target_os = "windows") {
        &["llmfit-windows.exe", "llmfit.exe"]
    } else if cfg!(target_os = "macos") {
        &["llmfit-darwin", "llmfit"]
    } else {
        &["llmfit-linux", "llmfit"]
    };
    names.iter().map(|n| comp.join(n)).find(|p| p.exists())
}

/// Copy the pool's llmfit into the (writable) tools dir + make it executable, so it
/// runs even off a FAT32 USB (no exec bit). Falls back to the pool path on copy fail.
fn prepare_llmfit(comp: &Path, tools: &Path) -> Option<PathBuf> {
    let src = pool_llmfit(comp)?;
    let name = if cfg!(target_os = "windows") { "llmfit.exe" } else { "llmfit" };
    let dst = tools.join(name);
    let _ = fs::create_dir_all(tools);
    if fs::copy(&src, &dst).is_err() {
        return Some(src);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
    }
    Some(dst)
}

const LLMFIT_PORT: &str = "8787";

/// Run `llmfit system --json` (GPU/VRAM/backend) → pass the raw JSON to Electron via
/// PLANAI_GPU_JSON (the JS side parses it), and start `llmfit serve` (model browser
/// API the dashboard proxies). Returns the serve child so we can stop it on exit.
fn start_llmfit(lf: &Path) -> Option<std::process::Child> {
    if let Ok(out) = Command::new(lf).args(["system", "--json"]).output() {
        if out.status.success() {
            let json = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !json.is_empty() {
                std::env::set_var("PLANAI_GPU_JSON", json);
                log("llmfit: GPU detected (PLANAI_GPU_JSON set)");
            }
        }
    }
    match Command::new(lf)
        .args(["serve", "--host", "127.0.0.1", "--port", LLMFIT_PORT])
        .env("OLLAMA_HOST", "127.0.0.1:11434")
        .spawn()
    {
        Ok(child) => {
            std::env::set_var("PLANAI_LLMFIT_URL", format!("http://127.0.0.1:{LLMFIT_PORT}"));
            log(&format!("llmfit serve on 127.0.0.1:{LLMFIT_PORT}"));
            Some(child)
        }
        Err(e) => {
            log(&format!("llmfit serve failed to start: {e}"));
            None
        }
    }
}

/// Default socket the control plane and supervisor meet on.
fn supervisor_socket_path() -> PathBuf {
    cache_root().join("services.sock")
}

/// `plan-ai supervisor <socket>`: run the mac-mgmt-services process supervisor
/// (spawns + restarts + log-streams the services the control plane registers).
fn run_supervisor(socket: &Path) -> ! {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => { log(&format!("supervisor runtime: {e}")); std::process::exit(1); }
    };
    match rt.block_on(mac_mgmt_services::server::run(socket)) {
        Ok(_) => std::process::exit(0),
        Err(e) => { log(&format!("supervisor: {e}")); std::process::exit(1); }
    }
}

/// `plan-ai serve-stack`: drive the rust control plane standalone (spawn the
/// supervisor + register ollama + open-webui, wait for health, report, shut down).
/// A smoke test of the control plane; the live wiring lands with the thin Electron.
fn run_serve_stack() -> ! {
    config::init_ports();
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => { log(&format!("control runtime: {e}")); std::process::exit(1); }
    };
    let code = rt.block_on(async {
        let self_exe = std::env::current_exe().expect("current_exe");
        let socket = supervisor_socket_path();
        let mut client = match control::start_stack(&self_exe, &socket).await {
            Ok(c) => c,
            Err(e) => { log(&format!("control: {e}")); return 1; }
        };
        log("control: services registered; waiting for health (≤120s)…");
        let (ollama, webui) = control::await_healthy(std::time::Duration::from_secs(120)).await;
        log(&format!("control: ollama={ollama} open-webui={webui}"));
        let _ = client.shutdown().await;
        if ollama && webui { 0 } else { 1 }
    });
    std::process::exit(code);
}

/// `plan-ai serve`: start the control plane + serve the SPA + control API over
/// localhost; print/export PLANAI_UI_URL and run until signalled. Electron (thin
/// webview, phase 5) loads this URL.
fn run_serve() -> ! {
    config::init_ports();
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => { log(&format!("serve runtime: {e}")); std::process::exit(1); }
    };
    let code = rt.block_on(async {
        let self_exe = std::env::current_exe().expect("current_exe");
        let socket = supervisor_socket_path();
        let client = match control::start_stack(&self_exe, &socket).await {
            Ok(c) => c,
            Err(e) => { log(&format!("control: {e}")); return 1; }
        };
        let port: u16 = std::env::var("PLANAI_UI_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8088);
        match serve::run_server(client, port).await {
            Ok(url) => {
                std::env::set_var("PLANAI_UI_URL", &url);
                log(&format!("UI server on {url}"));
            }
            Err(e) => { log(&format!("serve: {e}")); return 1; }
        }
        let _ = tokio::signal::ctrl_c().await;
        0
    });
    std::process::exit(code);
}

fn main() {
    // Subcommands (re-invocations of this same binary):
    {
        let mut a = std::env::args_os().skip(1);
        match a.next().as_deref().and_then(|s| s.to_str()) {
            Some("supervisor") => {
                let socket = a.next().map(PathBuf::from).unwrap_or_else(supervisor_socket_path);
                run_supervisor(&socket);
            }
            Some("serve-stack") => run_serve_stack(),
            Some("serve") => run_serve(),
            _ => {}
        }
    }

    let exe = std::env::current_exe().expect("current_exe");
    let here = exe.parent().expect("exe parent").to_path_buf();

    // Are we the re-executed copy running inside the NixOS FHS sandbox? If so the
    // host parent already took the lock and mounted the components — we just run.
    let in_fhs = std::env::var_os("PLANAI_FHS_REEXEC").is_some();

    let mut mounts: Vec<Mount> = Vec::new();
    // If something upstream already prepared the resources, don't touch them.
    let mut resources = std::env::var_os("PLANAI_RESOURCES").map(PathBuf::from);
    // The app component is mounted/linked here so we can run Electron from it.
    let mut app_dir: Option<PathBuf> = None;
    let comp_dir = components_dir(&here);

    // Single-instance guard — taken by the host process only (the FHS child is
    // part of the same run). A second launch would fight over the supervisor
    // socket + ports, so bail out. Held for the whole run (OS frees it on exit).
    let _instance_lock = if in_fhs {
        None
    } else {
        match acquire_instance_lock() {
            Ok(f) => Some(f),
            Err(true) => {
                log("another plan.ai instance is already running — exiting");
                notify("plan.ai", "plan.ai is already running.");
                std::process::exit(0);
            }
            Err(false) => None, // couldn't create the lock file — proceed unguarded
        }
    };

    // Choose ollama/open-webui ports up front (host picks; the supervisor + FHS
    // child inherit via env). Falls back off 11434/8080 when a host service holds
    // them, so the bundled ollama never crash-loops on "address in use".
    config::init_ports();

    // Prepare components on the HOST (the FHS child skips this — it inherits
    // PLANAI_RESOURCES). Mounting here means FUSE uses the host's fusermount +
    // /dev/fuse; the FHS sandbox then sees the mounts through its recursive bind,
    // so NixOS gets a real mount instead of a slow extraction.
    if !in_fhs && resources.is_none() {
        if let Some(comp) = comp_dir.as_ref() {
            let root = cache_root().join("root");
            let dist = root.join("dist");
            let tools = root.join("tools");
            let _ = fs::create_dir_all(&dist);
            // FUSE-mount first on every platform — incl. NixOS. We're on the host
            // here (before the FHS sandbox), so the static squashfuse_ll has the
            // host's fusermount + /dev/fuse and the mount succeeds; the FHS child
            // sees it via the recursive bind. provide() still falls back to
            // unsquashfs extraction if the mount fails. PLANAI_FORCE_EXTRACT forces it.
            let force_extract = std::env::var_os("PLANAI_FORCE_EXTRACT").is_some();
            if force_extract {
                log("PLANAI_FORCE_EXTRACT set — extracting components (no FUSE mount)");
            } else if is_nixos() {
                log("NixOS — FUSE-mounting components on the host (extract fallback)");
            }

            let rt = pick_base(comp, "runtime-").unwrap_or_else(|| {
                log("no runtime component found");
                std::process::exit(1);
            });
            match provide(comp, &rt, &dist.join("runtime"), &tools, force_extract) {
                Ok(k) => mounts.push(Mount { dest: dist.join("runtime"), kind: k }),
                Err(e) => { log(&format!("runtime: {e}")); std::process::exit(1); }
            }
            if comp.join("ow-assets.squashfs").exists() || comp.join("ow-assets.dmg").exists() || comp.join("ow-assets").is_dir() {
                if let Ok(k) = provide(comp, "ow-assets", &dist.join("ow-assets"), &tools, force_extract) {
                    mounts.push(Mount { dest: dist.join("ow-assets"), kind: k });
                }
            }
            if let Some((ol, why)) = detect_ollama(comp) {
                log(&format!("ollama flavour: {ol} — {why}"));
                match provide(comp, &ol, &dist.join("ollama"), &tools, force_extract) {
                    Ok(k) => mounts.push(Mount { dest: dist.join("ollama"), kind: k }),
                    Err(e) => log(&format!("ollama: {e}")),
                }
                std::env::set_var("PLANAI_OLLAMA_FLAVOUR", ol);
                std::env::set_var("PLANAI_OLLAMA_REASON", why);
            }
            // The Electron app itself ships as a component (app-<os>): mount/link it
            // and run Electron from the mounted tree (never extract — Electron runs
            // fine read-only). NixOS still needs a writable tree for its loader bits.
            if let Some(app) = pick_base(comp, "app-") {
                match provide(comp, &app, &dist.join("app"), &tools, force_extract) {
                    Ok(k) => { mounts.push(Mount { dest: dist.join("app"), kind: k }); app_dir = Some(dist.join("app")); }
                    Err(e) => log(&format!("app: {e}")),
                }
            }
            std::env::set_var("PLANAI_RESOURCES", &dist);
            // Electron's node loader finds the shared pool via this (it no longer
            // sits beside the running app — the app runs from <cache>/dist/app).
            std::env::set_var("PLANAI_COMPONENTS", comp);
            resources = Some(dist);
        }
    }
    // Inside the FHS child the host already mounted everything; locate the app
    // tree under the inherited PLANAI_RESOURCES so we can find Electron.
    if in_fhs {
        if let Some(res) = resources.as_ref() {
            let a = res.join("app");
            if a.exists() {
                app_dir = Some(a);
            }
        }
    }
    let _ = resources;

    // NixOS prod: now that the components are mounted on the host, run the stack
    // inside the FHS sandbox as a child (it sees the mounts via the recursive
    // bind), wait for it, then unmount on the host. No-op on non-NixOS / dev /
    // when already inside the sandbox.
    #[cfg(target_os = "linux")]
    if !in_fhs {
        if let Some(code) = maybe_run_in_fhs(comp_dir.as_deref()) {
            teardown(&mounts);
            flush_drive(); // after unmount: persist the USB + "safe to unplug"
            std::process::exit(code);
        }
    }

    // llmfit: GPU detection (→ PLANAI_GPU_JSON for Electron) + the model-browser
    // serve API (→ PLANAI_LLMFIT_URL, proxied by the dashboard). The binary comes
    // from PLANAI_LLMFIT (dev) or the shared pool (prod). Best-effort.
    let mut llmfit_child: Option<std::process::Child> = None;
    {
        let lf = std::env::var_os("PLANAI_LLMFIT")
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .or_else(|| comp_dir.as_ref().and_then(|c| {
                prepare_llmfit(c, &cache_root().join("root").join("tools"))
            }));
        if let Some(lf) = lf {
            llmfit_child = start_llmfit(&lf);
        }
    }

    // Prefer the Electron binary inside the mounted app component; fall back to one
    // sitting beside the launcher (back-compat).
    let program = app_dir
        .as_deref()
        .and_then(electron_in)
        .or_else(|| electron_target(&here));
    let program = match program {
        Some(p) => p,
        None => {
            log(&format!("could not find the bundled app (no app-<os> component, none beside {})", here.display()));
            teardown(&mounts);
            std::process::exit(1);
        }
    };

    // Rust control plane (phase 5): start the supervisor + register ollama +
    // open-webui, then serve the embedded Dioxus SPA + control API over
    // localhost. Electron becomes a thin webview that loads PLANAI_UI_URL — the
    // node supervisor/loader/renderer are gone. The server tasks run on this
    // runtime (kept alive for the whole electron session).
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => { log(&format!("runtime: {e}")); teardown(&mounts); std::process::exit(1); }
    };
    let socket = supervisor_socket_path();
    let self_exe = exe.clone();
    rt.block_on(async {
        match control::start_stack(&self_exe, &socket).await {
            Ok(client) => {
                let port: u16 = std::env::var("PLANAI_UI_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8088);
                match serve::run_server(client, port).await {
                    Ok(url) => { std::env::set_var("PLANAI_UI_URL", &url); log(&format!("UI server on {url}")); }
                    Err(e) => log(&format!("serve: {e} — UI may be unavailable")),
                }
            }
            Err(e) => log(&format!("control plane: {e} — services may be unavailable")),
        }
    });

    // Run Electron (thin webview). It inherits the environment (incl.
    // PLANAI_UI_URL) and user args; we tear the mounts down on exit.
    let mut cmd = Command::new(&program);
    // Dev: the nixpkgs Electron (PLANAI_ELECTRON) needs the app DIR as its first
    // arg; the bundled Electron has the app baked in, so PLANAI_ELECTRON_APP is
    // unset there.
    if let Some(appdir) = std::env::var_os("PLANAI_ELECTRON_APP") {
        cmd.arg(appdir);
    }
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

    // Stop the supervisor (it stops its managed children on shutdown), then the
    // llmfit serve, then release the component mounts.
    rt.block_on(async {
        if let Ok(mut c) = mac_mgmt_services::Client::connect(&socket, std::time::Duration::from_secs(5)).await {
            let _ = c.shutdown().await;
        }
    });
    if let Some(mut c) = llmfit_child {
        let _ = c.kill();
        let _ = c.wait();
    }
    teardown(&mounts);
    // Flush + "safe to unplug" after unmount. Skip in the in-FHS child — its host
    // parent does the teardown+flush after we exit (avoids a double notification).
    if !in_fhs {
        flush_drive();
    }
    std::process::exit(code);
}
