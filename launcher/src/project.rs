//! The plan.ai project plugged into the shared loader lifecycle.
//!
//! [`loader_core::lifecycle`] owns the phase sequencing, the NixOS FHS-child
//! delegation, the updater + splash plumbing, and the apply/relaunch decisions.
//! Everything genuinely plan.ai-specific — which components to mount (runtime /
//! ow-assets / hermes / ollama / llamacpp / app / usbd, with GPU-flavour
//! detection), the session it runs (llmfit + usbd/supervisor + the SPA control
//! API + Electron), and the localized splash text — lives here, behind the
//! [`loader_core::lifecycle::Project`] trait.

use std::path::PathBuf;
use std::process::Command;

use loader_core::lifecycle::{PrepareCtx, Project, SessionCtx};

use crate::{
    cache_root, control, detect_llamacpp, detect_ollama, electron_in, electron_target,
    kill_spinner, log, pick_base, prepare_llmfit, serve, start_llmfit, supervisor_socket_path,
    update, usbd, Mount,
};

/// `provide()` a component, unless a dev override dir is set for this `slot`
/// (`PLANAI_OVERRIDE_<slot>`, e.g. via `--with <slot>=<dir>`) — then link that local
/// folder in place instead. The "replace this component with this folder" logic is
/// generic (it lives in loader-core); this is just the project's call into it.
use loader_core::provide_or_override as mount;

/// The plan.ai run. Holds the session-scoped handles the lifecycle's teardown
/// must stop (the daemon/supervisor, llmfit, the tokio runtime); the generic
/// run state (mounts, spinner, updater, …) lives in [`loader_core::lifecycle::Ctx`].
pub struct PlanAi {
    socket: PathBuf,
    usbd_started: Option<(PathBuf, PathBuf, std::process::Child)>,
    llmfit_child: Option<std::process::Child>,
    rt: Option<tokio::runtime::Runtime>,
}

impl PlanAi {
    pub fn new() -> Self {
        PlanAi { socket: supervisor_socket_path(), usbd_started: None, llmfit_child: None, rt: None }
    }
}

/// The host-local cache leaf dir (`~/.cache/<name>`, `%LOCALAPPDATA%\<name>`, …).
/// Single source for both the Product impl and the early `main()` set (so CLI
/// subcommands that touch the cache before the lifecycle agree).
pub const CACHE_DIR_NAME: &str = "plan-ai-node-mgmt";

impl Project for PlanAi {
    fn brand(&self) -> &str {
        "plan.ai node-mgmt"
    }

    fn cache_dir_name(&self) -> &str {
        CACHE_DIR_NAME
    }

    /// Mount the components on the HOST and export the env the stack inherits.
    /// Mounting here means FUSE uses the host's fusermount + /dev/fuse; the FHS
    /// sandbox then sees the mounts through its recursive bind. The FHS child
    /// only resolves its app dir from the inherited PLANAI_RESOURCES.
    fn prepare_mounts(&mut self, ctx: &mut PrepareCtx) -> bool {
        if ctx.in_fhs {
            if let Some(res) = ctx.resources {
                let a = res.join("app");
                if a.exists() {
                    *ctx.app_dir = Some(a);
                }
            }
            return true;
        }
        let Some(comp) = ctx.comp else { return true };
        let (dist, tools, force_extract) = (ctx.dist, ctx.tools, ctx.force_extract);

        // The python runtime + ow-assets ARE the "openwebui" feature: with it
        // disabled (or the component pruned) we run without Open-WebUI instead
        // of exiting — ollama + the dashboard still work.
        let features = update::read_selection().features;
        let openwebui_on = features.iter().any(|f| f == "openwebui");
        match pick_base(comp, "runtime-") {
            Some(rt) if openwebui_on => match mount(comp, &rt, &dist.join("runtime"), "runtime", tools, force_extract) {
                Ok(k) => ctx.mounts.push(Mount { dest: dist.join("runtime"), kind: k }),
                Err(e) => {
                    log(&format!("runtime: {e}"));
                    return false;
                }
            },
            Some(_) => log("openwebui feature disabled — skipping the runtime mount"),
            None if openwebui_on => log("no runtime component found — running without Open-WebUI"),
            None => {}
        }
        if openwebui_on
            && (comp.join("ow-assets.squashfs").exists() || comp.join("ow-assets.dmg").exists() || comp.join("ow-assets").is_dir())
        {
            if let Ok(k) = mount(comp, "ow-assets", &dist.join("ow-assets"), "ow-assets", tools, force_extract) {
                ctx.mounts.push(Mount { dest: dist.join("ow-assets"), kind: k });
            }
        }
        // hermes: the optional agent component (feature "hermes", default-off).
        if features.iter().any(|f| f == "hermes")
            && (comp.join("hermes.squashfs").exists() || comp.join("hermes.dmg").exists() || comp.join("hermes").is_dir())
        {
            match mount(comp, "hermes", &dist.join("hermes"), "hermes", tools, force_extract) {
                Ok(k) => ctx.mounts.push(Mount { dest: dist.join("hermes"), kind: k }),
                Err(e) => log(&format!("hermes: {e}")),
            }
        }
        // hermes-webui: the lightweight hermes web UI (same feature) — pure
        // python/static sources run with the hermes component's python.
        if features.iter().any(|f| f == "hermes")
            && (comp.join("hermes-webui.squashfs").exists() || comp.join("hermes-webui.dmg").exists() || comp.join("hermes-webui").is_dir())
        {
            match mount(comp, "hermes-webui", &dist.join("hermes-webui"), "hermes-webui", tools, force_extract) {
                Ok(k) => ctx.mounts.push(Mount { dest: dist.join("hermes-webui"), kind: k }),
                Err(e) => log(&format!("hermes-webui: {e}")),
            }
        }
        if let Some((ol, why)) = detect_ollama(comp) {
            log(&format!("ollama flavour: {ol} — {why}"));
            match mount(comp, &ol, &dist.join("ollama"), "ollama", tools, force_extract) {
                Ok(k) => ctx.mounts.push(Mount { dest: dist.join("ollama"), kind: k }),
                Err(e) => log(&format!("ollama: {e}")),
            }
            std::env::set_var("PLANAI_OLLAMA_FLAVOUR", ol);
            std::env::set_var("PLANAI_OLLAMA_REASON", why);
        }
        // llama.cpp (optional feature): GPU-detected flavour, mounted at a
        // flavour-neutral dist/llamacpp like ollama's dist/ollama.
        if features.iter().any(|f| f == "llamacpp") {
            if let Some((lc, why)) = detect_llamacpp(comp) {
                log(&format!("llamacpp flavour: {lc} — {why}"));
                match mount(comp, &lc, &dist.join("llamacpp"), "llamacpp", tools, force_extract) {
                    Ok(k) => ctx.mounts.push(Mount { dest: dist.join("llamacpp"), kind: k }),
                    Err(e) => log(&format!("llamacpp: {e}")),
                }
                std::env::set_var("PLANAI_LLAMACPP_FLAVOUR", lc);
                std::env::set_var("PLANAI_LLAMACPP_REASON", why);
            }
        }
        // The Electron app itself ships as a component (app-<os>): mount/link it
        // and run Electron from the mounted tree. Dev override
        // (`--with-electron DIR` → PLANAI_APP_DIR): use a local unpacked
        // app tree instead of mounting the component.
        if let Some(dir) = std::env::var_os("PLANAI_APP_DIR").map(PathBuf::from) {
            if dir.is_dir() {
                log(&format!("app: dev override — {}", dir.display()));
                *ctx.app_dir = Some(dir);
            } else {
                log(&format!("app: PLANAI_APP_DIR {} is not a directory — ignoring", dir.display()));
            }
        } else if let Some(app) = pick_base(comp, "app-") {
            match mount(comp, &app, &dist.join("app"), "app", tools, force_extract) {
                Ok(k) => {
                    ctx.mounts.push(Mount { dest: dist.join("app"), kind: k });
                    *ctx.app_dir = Some(dist.join("app"));
                }
                Err(e) => log(&format!("app: {e}")),
            }
        }
        // The plan.ai USB daemon (usbd → dist/usbd/mac-mgmt[.exe]). Optional —
        // absent in dev / older bundles, where the launcher falls back to its
        // own supervisor.
        if comp.join("usbd.squashfs").exists() || comp.join("usbd.dmg").exists() || comp.join("usbd").is_dir() {
            match mount(comp, "usbd", &dist.join("usbd"), "usbd", tools, force_extract) {
                Ok(k) => ctx.mounts.push(Mount { dest: dist.join("usbd"), kind: k }),
                Err(e) => log(&format!("usbd: {e}")),
            }
        }
        std::env::set_var("PLANAI_RESOURCES", dist);
        // Electron's node loader finds the shared pool via this (the app runs
        // from <cache>/dist/app, not from beside the pool).
        std::env::set_var("PLANAI_COMPONENTS", comp);
        true
    }

    /// The session: llmfit + usbd/supervisor + UI server + Electron, until the
    /// app quits (or /api/update/apply kills it to request an apply).
    fn run_session(&mut self, ctx: &mut SessionCtx) -> i32 {
        // llmfit: GPU detection (→ PLANAI_GPU_JSON for Electron) + the
        // model-browser serve API. Best-effort.
        {
            let lf = std::env::var_os("PLANAI_LLMFIT")
                .map(PathBuf::from)
                .filter(|p| p.exists())
                .or_else(|| {
                    ctx.comp_dir.and_then(|c| prepare_llmfit(c, &cache_root().join("root").join("tools")))
                });
            if let Some(lf) = lf {
                self.llmfit_child = start_llmfit(&lf);
                // Register it in the process sidecar so teardown's reaper has it.
                if let Some(c) = self.llmfit_child.as_ref() {
                    loader_core::proc::track(c.id(), "llmfit");
                }
            }
        }

        // Prefer the Electron binary inside the mounted app component; fall
        // back to one sitting beside the launcher (back-compat).
        let program = ctx
            .app_dir
            .and_then(electron_in)
            .or_else(|| electron_target(ctx.here));
        let program = match program {
            Some(p) => p,
            None => {
                log(&format!(
                    "could not find the bundled app (no app-<os> component, none beside {})",
                    ctx.here.display()
                ));
                return 1;
            }
        };

        // Rust control plane: prefer the plan.ai USB daemon when shipped; else
        // the launcher's own supervisor. Spawn BEFORE the tokio runtime
        // (single-threaded) so the env it inherits is set safely.
        self.usbd_started = usbd::resolve_bin().and_then(|bin| usbd::spawn(&bin));

        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                log(&format!("runtime: {e}"));
                return 1;
            }
        };
        let self_exe = ctx.exe.to_path_buf();
        let socket = self.socket.clone();
        let spinner = ctx.spinner.clone();
        let updater = ctx.updater.clone();
        let apply_requested = ctx.apply_requested.clone();
        let electron = ctx.app.clone();
        let usbd_sock = self.usbd_started.as_ref().map(|(_h, s, _c)| s.clone());
        rt.block_on(async {
            // Daemon mode: connect to the daemon's in-process supervisor socket.
            // Dev mode: start the launcher's own supervisor + register services.
            let client = match &usbd_sock {
                Some(sock) => {
                    match mac_mgmt_services::Client::connect(sock, std::time::Duration::from_secs(30)).await {
                        Ok(c) => {
                            log("control: usb daemon owns the supervisor");
                            Ok(c)
                        }
                        Err(e) => Err(format!("usb daemon socket: {e}")),
                    }
                }
                None => control::start_stack(&self_exe, &socket).await.map_err(|e| e.to_string()),
            };
            match client {
                Ok(client) => {
                    let port: u16 =
                        std::env::var("PLANAI_UI_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8088);
                    match serve::run_server(client, port, spinner, updater, apply_requested, electron).await {
                        Ok(url) => {
                            std::env::set_var("PLANAI_UI_URL", &url);
                            log(&format!("UI server on {url}"));
                        }
                        Err(e) => log(&format!("serve: {e} — UI may be unavailable")),
                    }
                }
                Err(e) => log(&format!("control plane: {e} — services may be unavailable")),
            }
        });

        // First-run bootstrap: fetch the manifest + pre-download in the
        // background. Routine checks are manual (the SPA's "Check for updates").
        if ctx.bootstrap_update {
            log("no update manifest on the drive — bootstrapping from the update server");
            rt.spawn(update::check_and_predownload(ctx.updater.clone()));
        }
        self.rt = Some(rt);

        // Run Electron (thin webview). It inherits the environment (incl.
        // PLANAI_UI_URL) and user args.
        let mut cmd = Command::new(&program);
        // Dev: the nixpkgs Electron (PLANAI_ELECTRON) needs the app DIR as its
        // first arg; the bundled Electron has the app baked in.
        if let Some(appdir) = std::env::var_os("PLANAI_ELECTRON_APP") {
            cmd.arg(appdir);
        }
        #[cfg(target_os = "linux")]
        cmd.arg("--no-sandbox"); // read-only AppImage mount can't setuid chrome-sandbox
        cmd.args(ctx.args);

        // Safety net: the mount is done and the control plane is up, so
        // Electron should paint within seconds and POST /api/ready. If that
        // signal never lands, close the spinner so it can't sit over the session.
        {
            let h = ctx.spinner.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(30));
                kill_spinner(&h);
            });
        }

        // Spawn Electron into the shared handle so /api/update/apply can
        // terminate it (→ this wait returns → Teardown/Apply run with the
        // runtime down). Poll so the server task can take the lock to kill it.
        match cmd.spawn() {
            Ok(child) => {
                loader_core::proc::track(child.id(), "electron");
                *ctx.app.lock().unwrap() = Some(child);
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let mut g = ctx.app.lock().unwrap();
                    match g.as_mut().map(|c| c.try_wait()) {
                        Some(Ok(Some(st))) => break st.code().unwrap_or(0),
                        Some(Ok(None)) => continue, // still running
                        _ => break 0,
                    }
                }
            }
            Err(e) => {
                log(&format!("failed to start {}: {e}", program.display()));
                1
            }
        }
    }

    /// Stop the session's services in dependency order, so nothing is still
    /// executing from a mount when the lifecycle unmounts it: daemon/supervisor
    /// (their services run from the mounted trees) → llmfit. (The splash + the
    /// component mounts are torn down by the lifecycle around this call.)
    fn teardown_session(&mut self) {
        match self.usbd_started.take() {
            // usbd mode: SIGTERM the daemon and wait — it stops its services
            // and exits, releasing every reference into the pool.
            Some((_home, _sock, child)) => usbd::stop(child),
            // dev mode: the launcher's own in-process supervisor, via its socket.
            None => {
                if let Some(rt) = self.rt.as_ref() {
                    let socket = self.socket.clone();
                    rt.block_on(async {
                        if let Ok(mut c) =
                            mac_mgmt_services::Client::connect(&socket, std::time::Duration::from_secs(5)).await
                        {
                            let _ = c.shutdown().await;
                        }
                    });
                }
            }
        }
        if let Some(mut c) = self.llmfit_child.take() {
            loader_core::proc::untrack(c.id());
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
