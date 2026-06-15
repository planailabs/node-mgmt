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

use clap::Parser;

mod config;
mod control;
mod i18n;
mod lifecycle;
mod paths;
mod proxy;
mod serve;
mod usbd;

// The FHS-entry / component-mount / pool-discovery / splash / cache+lock substrate, the
// shared HTTP client, and the crash-safe self-updater (update + apply) now live in the
// loader-core runtime crate; re-export them so the lifecycle state machine + the project
// glue below keep referring to `crate::*`.
pub(crate) use loader_core::{apply, net, update};
pub(crate) use loader_core::{
    acquire_instance_lock, cache_root, components_dir, external_roots, is_nixos, kill_spinner,
    log, notify, pick_base, pool_drive_root, provide, show_splash, teardown, Mount,
    SpinnerHandle, SplashOpts,
};
#[cfg(target_os = "linux")]
pub(crate) use loader_core::maybe_run_in_fhs;


/// Shared handle to the Electron child so the update-apply endpoint can terminate
/// it (→ main's wait returns → apply runs). Held by main (waits) + the server.
pub type ElectronHandle = std::sync::Arc<std::sync::Mutex<Option<std::process::Child>>>;
/// Does the drive need (re)provisioning from the update server before we can run?
///   - first run: no component pool AND no local manifest (the original trigger), or
///   - repair: a local manifest exists but lists a component for THIS platform whose
///     on-disk artifact is gone (deleted / corrupted / a partial burn). Either way we
///     run the updater, which — because a component is missing — does a FULL re-fetch
///     to the remote version (update::local_for_plan), keeping the set consistent
///     instead of mixing a re-fetched component with stale siblings.
/// Pins PLANAI_PORTABLE_ROOT from the pool first so the on-disk check (and the apply
/// that follows) target the USB root, not the process cwd. A dev/flat pool without a
/// manifest still runs directly (no blocking network fetch) — preserving old behavior.
fn pool_needs_provision(comp_dir: Option<&PathBuf>) -> bool {
    if std::env::var_os("PLANAI_PORTABLE_ROOT").is_none() {
        if let Some(root) = comp_dir.and_then(|c| pool_drive_root(c)) {
            std::env::set_var("PLANAI_PORTABLE_ROOT", root);
        }
    }
    match update::load_local() {
        None => comp_dir.is_none(),
        Some(m) => {
            let sel = update::read_selection();
            let root = paths::portable_root();
            let missing = m.files.iter().any(|e| e.wanted_by(&sel) && !update::artifact_present(&root, e));
            if missing {
                log("components missing on the drive — repairing from the update server");
            }
            missing
        }
    }
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

/// Pick the llama.cpp flavour for this machine (optional "llamacpp" feature),
/// mirroring detect_ollama: vulkan when a GPU render node + the vulkan loader
/// are present (linux) / vulkan-1.dll exists (windows), else the cpu build;
/// mac is Metal-always. PLANAI_LLAMACPP=cpu forces the cpu build.
fn detect_llamacpp(comp: &Path) -> Option<(String, String)> {
    let has = |key: &str| {
        ["squashfs", "tar.gz", "dmg"]
            .iter()
            .any(|e| comp.join(format!("llamacpp-{key}.{e}")).exists())
            || comp.join(format!("llamacpp-{key}")).is_dir()
    };
    let pick = |key: &str, why: &str| Some((format!("llamacpp-{key}"), why.to_string()));
    let force_cpu = std::env::var("PLANAI_LLAMACPP").as_deref() == Ok("cpu");
    if cfg!(target_os = "linux") {
        let arch = if std::env::consts::ARCH == "aarch64" { "linux-arm64" } else { "linux-amd64" };
        let gpu = std::fs::read_dir("/dev/dri")
            .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("renderD")))
            .unwrap_or(false);
        let vulkan = format!("{arch}-vulkan");
        if !force_cpu && gpu && has(&vulkan) && lib_present(&["libvulkan.so.1", "libvulkan.so"]) {
            return pick(&vulkan, "GPU render node + vulkan loader detected");
        }
        if has(arch) {
            return pick(arch, "CPU (default)");
        }
    } else if cfg!(target_os = "macos") {
        if has("darwin") {
            return pick("darwin", "macOS arm64 (Metal)");
        }
    } else if cfg!(target_os = "windows") {
        let vulkan_dll = Path::new("C:\\Windows\\System32\\vulkan-1.dll").exists();
        if !force_cpu && vulkan_dll && has("windows-amd64-vulkan") {
            return pick("windows-amd64-vulkan", "vulkan-1.dll present");
        }
        if has("windows-amd64") {
            return pick("windows-amd64", "Windows x64 CPU");
        }
    }
    None
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
    notify("plan.ai", &i18n::t("safe-to-unplug"));
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
        // On copy failure llmfit runs from the pool (comp) dir, where the bundled
        // VCRUNTIME140.dll already sits beside it — so the windows loader still
        // resolves the redist there; nothing extra to do for the fallback.
        return Some(src);
    }
    // Windows: the msvc-linked llmfit.exe dynamically links VCRUNTIME140.dll. We run
    // it from the writable `tools` dir (FAT32 has no exec bit), so the redist DLLs the
    // pool ships beside it must be copied next to the destination .exe (the loader
    // searches the exe's own dir first), or it dies with "VCRUNTIME140.dll not found".
    #[cfg(target_os = "windows")]
    if let Some(dir) = src.parent() {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")) {
                    if let Some(fname) = p.file_name() {
                        let _ = fs::copy(&p, tools.join(fname));
                    }
                }
            }
        }
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
    // The download fallback (serve.rs run_download) shells back into llmfit.
    std::env::set_var("PLANAI_LLMFIT_BIN", lf);
    match Command::new(lf)
        .args(["serve", "--host", "127.0.0.1", "--port", LLMFIT_PORT])
        // the CONFIGURED ollama port, not a hardcoded 11434 — the stick's
        // server may run elsewhere
        .env("OLLAMA_HOST", format!("{}:{}", config::OLLAMA_HOST, config::ollama_port()))
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
        // Standalone serve mode owns no spinner/electron (empty handles → no-ops).
        let no_spinner: SpinnerHandle = std::sync::Arc::new(std::sync::Mutex::new(None));
        let updater = update::Updater::new();
        let apply_req = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let no_electron: ElectronHandle = std::sync::Arc::new(std::sync::Mutex::new(None));
        match serve::run_server(client, port, no_spinner, updater, apply_req, no_electron).await {
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

/// `plan-ai self-update`: check the update server, pre-download the delta, resume
/// any interrupted apply, then apply the staged update. For ops + integration tests
/// (drive it with PLANAI_PORTABLE_ROOT + PLANAI_CACHE + a local update server).
fn run_self_update() -> ! {
    apply::resume_if_interrupted(&i18n::t("applying-update"));
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let up = update::Updater::new();
    rt.block_on(update::check_and_predownload(up.clone()));
    let st = up.status();
    use plan_ai_control_api::UpdateState;
    log(&format!("self-update: state={:?} {}/{}", st.state, st.done, st.total));
    if st.state == UpdateState::Ready {
        let applied = apply::run(&up, &i18n::t("applying-update"));
        log(&format!("self-update: applied={applied}"));
        std::process::exit(if applied { 0 } else { 1 });
    }
    std::process::exit(if st.state == UpdateState::Idle { 0 } else { 1 });
}

/// Locate the prepared component tree (PLANAI_RESOURCES). A running launcher mounts
/// it at `<cache>/root/dist` and exports the env; a standalone CLI invocation
/// (`plan-ai ollama …`) inherits neither, so fall back to that well-known path and
/// export it so `paths::*` / `usbd::resolve_bin` resolve. None if nothing's mounted.
fn ensure_resources() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PLANAI_RESOURCES").map(PathBuf::from).filter(|p| p.exists()) {
        return Some(p);
    }
    let dist = cache_root().join("root").join("dist");
    if dist.exists() {
        std::env::set_var("PLANAI_RESOURCES", &dist);
        // models/ + data/ live beside the cache root's runtime; the daemon seeds the
        // portable root too. Only set if unset so an explicit override wins.
        return Some(dist);
    }
    None
}

/// The ollama port the running stack uses, so `plan-ai ollama …` talks to the live
/// server: PLANAI_OLLAMA_PORT if inherited, else the daemon's seeded config.json,
/// else the well-known default.
pub(crate) fn running_ollama_port() -> u16 {
    if let Some(p) = std::env::var("PLANAI_OLLAMA_PORT").ok().and_then(|p| p.parse().ok()) {
        return p;
    }
    let cfg = cache_root().join("usbd-home").join("config.json");
    if let Ok(s) = std::fs::read_to_string(&cfg) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            if let Some(p) = v.get("ollama").and_then(|o| o.get("port")).and_then(|p| p.as_u64()) {
                return p as u16;
            }
        }
    }
    config::OLLAMA_PORT_DEFAULT
}

/// Run a bundled tool (`ollama` / `mac-mgmt`) with the launcher's mounted runtime,
/// forwarding argv + stdio and exiting with the child's status. Never returns.
fn run_tool_passthrough(tool: &str, args: Vec<std::ffi::OsString>) -> ! {
    let mounted = ensure_resources().is_some();
    let bin = match tool {
        "ollama" if mounted => Some(paths::ollama_binary()).filter(|b| b.exists()),
        "usbd" | "mac-mgmt" => usbd::resolve_bin(),
        _ => None,
    };
    let bin = match bin {
        Some(b) => b,
        None => {
            log(&format!(
                "`plan-ai {tool}` unavailable: plan.ai's components aren't mounted — start the app first, or point PLANAI_RESOURCES at a prepared component tree"
            ));
            std::process::exit(127);
        }
    };
    let mut cmd = Command::new(&bin);
    cmd.args(&args);
    if tool == "ollama" {
        // Point the CLI at the already-running server + the USB models dir, so
        // `ollama pull/list/rm` operate on the same store the app uses.
        cmd.env("OLLAMA_HOST", format!("{}:{}", config::OLLAMA_HOST, running_ollama_port()));
        cmd.env("OLLAMA_MODELS", paths::models_dir());
        // NixOS dev: foreign-binary libs come via PLANAI_CHILD_LD_LIBRARY_PATH.
        if let Ok(extra) = std::env::var("PLANAI_CHILD_LD_LIBRARY_PATH") {
            if !extra.is_empty() {
                let v = match std::env::var("LD_LIBRARY_PATH") {
                    Ok(e) if !e.is_empty() => format!("{extra}:{e}"),
                    _ => extra,
                };
                cmd.env("LD_LIBRARY_PATH", v);
            }
        }
    }
    match cmd.status() {
        Ok(st) => std::process::exit(st.code().unwrap_or(1)),
        Err(e) => {
            log(&format!("failed to run {}: {e}", bin.display()));
            std::process::exit(1);
        }
    }
}

/// The launcher CLI. The default (no subcommand) runs the app: the dev-override
/// flags feed env overrides, and any trailing args are forwarded to Electron. The
/// subcommands are internal re-invocations of this same binary (supervisor/serve) or
/// ops/passthrough helpers; each never returns.
#[derive(clap::Parser)]
#[command(name = "plan-ai", about = "plan.ai launcher", args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(clap::Args)]
struct RunArgs {
    /// Dev: replace a pooled component <slot> with a local folder (repeatable),
    /// e.g. `--with app=./app --with runtime=./dist/runtime/linux-x64`.
    #[arg(long = "with", value_name = "SLOT=DIR")]
    with: Vec<String>,
    /// Dev: run a locally-built Electron app tree instead of the app component.
    #[arg(long, value_name = "DIR")]
    start_with_electron: Option<PathBuf>,
    /// Dev: serve a locally-built SPA instead of the embedded assets.
    #[arg(long, value_name = "DIR")]
    start_with_spa: Option<PathBuf>,
    /// Dev: run a locally-built usbd (component dir or the binary).
    #[arg(long, value_name = "PATH")]
    start_with_usbd: Option<PathBuf>,
    /// Extra args forwarded to Electron (use `--` first for leading flags).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    electron_args: Vec<std::ffi::OsString>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// internal: run the mac-mgmt supervisor on <socket>.
    #[command(hide = true)]
    Supervisor { socket: Option<PathBuf> },
    /// internal: serve the control API + SPA against an in-process supervisor.
    #[command(hide = true)]
    ServeStack,
    /// internal: serve the control API + SPA against an existing supervisor.
    #[command(hide = true)]
    Serve,
    /// Ops: check the update server, download the delta, and apply it (no Electron).
    SelfUpdate,
    /// Run the bundled ollama with the remaining args (e.g. `ollama pull llama3`).
    Ollama {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },
    /// Run the bundled usbd with the remaining args.
    Usbd {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },
    /// Run the bundled mac-mgmt with the remaining args.
    MacMgmt {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },
}

/// Apply the app-mode dev overrides to the process env (before the env snapshot, so a
/// post-update relaunch — which restores env0 — keeps them).
fn apply_dev_overrides(run: &RunArgs) {
    let canon = |d: &Path| d.canonicalize().unwrap_or_else(|_| d.to_path_buf());
    for spec in &run.with {
        let Some((name, dir)) = spec.split_once('=') else {
            log(&format!("--with: expected <slot>=<dir>, got {spec}"));
            std::process::exit(2);
        };
        let key = format!("PLANAI_OVERRIDE_{}", name.to_ascii_uppercase().replace('-', "_"));
        let p = canon(Path::new(dir));
        log(&format!("dev override: {key}={}", p.display()));
        std::env::set_var(&key, &p);
    }
    if let Some(d) = &run.start_with_electron {
        std::env::set_var("PLANAI_APP_DIR", canon(d));
    }
    if let Some(d) = &run.start_with_spa {
        std::env::set_var("PLANAI_SPA_DIR", canon(d));
    }
    if let Some(d) = &run.start_with_usbd {
        let mut p = canon(d);
        // accepts the unpacked component DIR or the binary directly.
        if p.is_dir() {
            let names: &[&str] = if cfg!(windows) { &["usbd.exe", "mac-mgmt.exe"] } else { &["usbd", "mac-mgmt"] };
            if let Some(bin) = names.iter().map(|n| p.join(n)).find(|c| c.exists()) {
                p = bin;
            }
        }
        std::env::set_var("PLANAI_USBD_BIN", p);
    }
}

fn main() {
    let cli = Cli::parse();
    // Internal subcommands / passthroughs — each runs and exits (never returns).
    if let Some(cmd) = cli.cmd {
        match cmd {
            Cmd::Supervisor { socket } => run_supervisor(&socket.unwrap_or_else(supervisor_socket_path)),
            Cmd::ServeStack => run_serve_stack(),
            Cmd::Serve => run_serve(),
            Cmd::SelfUpdate => run_self_update(),
            Cmd::Ollama { args } => run_tool_passthrough("ollama", args),
            Cmd::Usbd { args } => run_tool_passthrough("usbd", args),
            Cmd::MacMgmt { args } => run_tool_passthrough("mac-mgmt", args),
        }
    }

    // App mode: apply dev overrides to the env BEFORE snapshotting it.
    apply_dev_overrides(&cli.run);

    // Snapshot the Electron args + the environment BEFORE the run mutates anything: a
    // relaunch after an update apply must start from this state, not from the run's
    // (PLANAI_RESOURCES etc. would point a fresh launcher at torn-down mounts).
    let args = cli.run.electron_args;
    let env0: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();

    let exe = std::env::current_exe().expect("current_exe");
    let here = exe.parent().expect("exe parent").to_path_buf();

    // Are we the re-executed copy running inside the NixOS FHS sandbox? If so the
    // host parent already took the lock and mounted the components — we just run.
    let in_fhs = std::env::var_os("PLANAI_FHS_REEXEC").is_some();

    // Single-instance guard — taken by the host process only (the FHS child is
    // part of the same run). A second launch would fight over the supervisor
    // socket + ports, so bail out. Held until exit (or handed over on relaunch).
    let instance_lock = if in_fhs {
        None
    } else {
        match acquire_instance_lock() {
            Ok(f) => Some(f),
            Err(true) => {
                log("another plan.ai instance is already running — exiting");
                notify("plan.ai", &i18n::t("already-running"));
                std::process::exit(0);
            }
            Err(false) => None, // couldn't create the lock file — proceed unguarded
        }
    };

    // Choose ollama/open-webui ports up front (host picks; the supervisor + FHS
    // child inherit via env). Falls back off 11434/8080 when a host service holds
    // them, so the bundled ollama never crash-loops on "address in use".
    config::init_ports();

    // Everything else — provisioning, mounting, the session, teardown, update
    // apply, relaunch — is the lifecycle state machine (see lifecycle.rs).
    lifecycle::run(lifecycle::Ctx::new(exe, here, in_fhs, args, env0, instance_lock));
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("plan-ai").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn cli_covers_app_mode_dev_flags_subcommands_and_passthrough() {
        // bare invocation → app mode, no subcommand, no electron args
        let c = parse(&[]);
        assert!(c.cmd.is_none() && c.run.electron_args.is_empty() && c.run.with.is_empty());

        // dev overrides (app mode)
        let c = parse(&["--with", "app=./app", "--with", "runtime=./rt", "--start-with-spa", "./spa"]);
        assert!(c.cmd.is_none());
        assert_eq!(c.run.with, vec!["app=./app", "runtime=./rt"]);
        assert_eq!(c.run.start_with_spa.as_deref(), Some(Path::new("./spa")));

        // trailing electron args after `--`
        let c = parse(&["--", "--inspect", "--foo=bar"]);
        assert_eq!(c.run.electron_args, vec!["--inspect", "--foo=bar"]);

        // internal subcommands
        assert!(matches!(parse(&["supervisor", "/tmp/s.sock"]).cmd, Some(Cmd::Supervisor { socket: Some(_) })));
        assert!(matches!(parse(&["serve-stack"]).cmd, Some(Cmd::ServeStack)));
        assert!(matches!(parse(&["self-update"]).cmd, Some(Cmd::SelfUpdate)));

        // tool passthrough forwards the remaining args (incl. hyphen values)
        match parse(&["ollama", "pull", "llama3", "--verbose"]).cmd {
            Some(Cmd::Ollama { args }) => assert_eq!(args, vec!["pull", "llama3", "--verbose"]),
            _ => panic!("expected ollama passthrough"),
        }
    }
}
