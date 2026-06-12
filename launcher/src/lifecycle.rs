//! The launcher's run lifecycle as an explicit state machine.
//!
//! Every run is a walk through [`Phase`]s; [`run`] drives phase → phase until
//! `Done`. The transitions — including the update-apply and relaunch decisions
//! that used to be scattered across a 500-line `main` as ad-hoc flags — live in
//! one place, each phase a method on [`Ctx`]:
//!
//! ```text
//! Provision ─→ Prepare ─→ Delegate ─┐
//!                  └────→ Stack ────┤
//!                                   ▼
//!                               Teardown ─→ Apply ─→ Flush ─→ Relaunch ─→ Done
//!                                   └──────(no apply)──┘ └──(no update)──┘
//! ```
//!
//! Host vs FHS child: on NixOS the host process mounts the components, then
//! runs the whole Stack inside the FHS sandbox as a child process (Delegate);
//! that child re-enters this same machine with `in_fhs = true` and walks ONLY
//! Prepare → Stack → Teardown → Done. Drive-level work — provisioning, update
//! apply, flush, relaunch — belongs to the host: the child signals "the user
//! requested an update apply" with the [`APPLY_REQUESTED_EXIT`] sentinel exit
//! code, and the host applies from the pendrive marker after its teardown.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::{
    apply, components_dir, control, electron_in, electron_target, i18n, kill_spinner, log, notify,
    paths, pool_needs_provision, prepare_llmfit, serve, show_splash, supervisor_socket_path,
    update, usbd, ElectronHandle, Mount, SpinnerHandle, SplashOpts,
};
use crate::{cache_root, detect_llamacpp, detect_ollama, pick_base, pool_drive_root, provide, start_llmfit};

/// Exit code the FHS child uses for "the session ended because the user asked
/// to apply the staged update". The child can't apply it itself (drive work is
/// the host's, outside the sandbox); the host maps this back to a normal exit
/// and runs Apply. 75 = EX_TEMPFAIL, far from Electron's real exit codes.
pub const APPLY_REQUESTED_EXIT: i32 = 75;

/// One phase of a launcher run. Documented transitions only — every arrow in
/// the module diagram corresponds to exactly one `match` arm in [`run`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// First-run / repair: no component pool or missing wanted artifacts —
    /// blocking download + apply through the regular updater, with a gauge.
    Provision,
    /// Pin the portable root, finish any interrupted apply, mount the
    /// components, export the env the stack inherits. The FHS child only
    /// resolves its app dir from the inherited `PLANAI_RESOURCES` here.
    Prepare,
    /// NixOS host: run the Stack inside the FHS sandbox as a child process and
    /// wait for it (the mounts stay visible through the recursive bind).
    Delegate,
    /// The session itself: usbd/supervisor + UI server + Electron, until quit.
    Stack,
    /// Stop services, kill helpers, unmount components — in dependency order.
    Teardown,
    /// Apply the staged update now that the runtime is fully down.
    Apply,
    /// Flush drive write buffers + "safe to unplug" notification.
    Flush,
    /// Land the user on the new version: re-exec the updated binary where
    /// possible, otherwise notify them to restart manually.
    Relaunch,
    /// Exit the process with the session's exit code (sentinel-mapped for the
    /// FHS child).
    Done,
}

/// Everything a run carries between phases. Built once in `main`, owned by the
/// machine until exit.
pub struct Ctx {
    /// The launcher executable path captured at startup (NOT re-read later:
    /// after a self-replace `/proc/self/exe` reads "<path> (deleted)").
    pub exe: PathBuf,
    /// `exe`'s directory (component pool discovery starts here).
    pub here: PathBuf,
    /// Are we the re-executed copy inside the NixOS FHS sandbox?
    pub in_fhs: bool,
    /// argv[1..] at startup, forwarded to Electron and to a relaunch.
    pub args: Vec<OsString>,
    /// The process environment AT STARTUP. A relaunch uses this, not the live
    /// environment — the run mutates PLANAI_RESOURCES / PLANAI_COMPONENTS /
    /// PLANAI_PORTABLE_ROOT etc., which would poison a fresh launcher (it
    /// would skip mounting and point at torn-down paths).
    pub env0: Vec<(OsString, OsString)>,
    /// Single-instance guard; dropped explicitly before a relaunch so the new
    /// process doesn't race the OS-level release on exit.
    pub instance_lock: Option<std::fs::File>,

    pub spinner: SpinnerHandle,
    pub updater: update::Handle,
    pub apply_requested: Arc<AtomicBool>,
    pub electron: ElectronHandle,

    // Filled in as phases run:
    comp_dir: Option<PathBuf>,
    resources: Option<PathBuf>,
    app_dir: Option<PathBuf>,
    mounts: Vec<Mount>,
    usbd_started: Option<(PathBuf, PathBuf, std::process::Child)>,
    llmfit_child: Option<std::process::Child>,
    rt: Option<tokio::runtime::Runtime>,
    socket: PathBuf,
    /// The session's exit code (Electron's, or an early-failure 1).
    exit_code: i32,
    /// Did Apply actually place an update onto the drive?
    applied: bool,
    /// First-run bootstrap: kick a background update check once the server is
    /// up. Decided in Prepare, BEFORE anything creates platforms.json as a
    /// side effect (read_selection does) — afterwards the signal is gone.
    bootstrap_update: bool,
}

impl Ctx {
    pub fn new(
        exe: PathBuf,
        here: PathBuf,
        in_fhs: bool,
        args: Vec<OsString>,
        env0: Vec<(OsString, OsString)>,
        instance_lock: Option<std::fs::File>,
    ) -> Self {
        let comp_dir = components_dir(&here);
        Ctx {
            exe,
            here,
            in_fhs,
            args,
            env0,
            instance_lock,
            spinner: Arc::new(std::sync::Mutex::new(None)),
            updater: update::Updater::new(),
            apply_requested: Arc::new(AtomicBool::new(false)),
            electron: Arc::new(std::sync::Mutex::new(None)),
            comp_dir,
            resources: std::env::var_os("PLANAI_RESOURCES").map(PathBuf::from),
            app_dir: None,
            mounts: Vec::new(),
            usbd_started: None,
            llmfit_child: None,
            rt: None,
            socket: supervisor_socket_path(),
            exit_code: 0,
            applied: false,
            bootstrap_update: false,
        }
    }
}

/// Drive the machine to completion. Never returns.
pub fn run(mut ctx: Ctx) -> ! {
    // The FHS child runs the stack against the host's mounts; everything
    // drive-level happened (or will happen) on the host.
    let mut phase = if ctx.in_fhs { Phase::Prepare } else { Phase::Provision };
    loop {
        log(&format!("lifecycle: {phase:?}"));
        phase = match phase {
            Phase::Provision => ctx.provision(),
            Phase::Prepare => ctx.prepare(),
            Phase::Delegate => ctx.delegate(),
            Phase::Stack => ctx.stack(),
            Phase::Teardown => ctx.teardown(),
            Phase::Apply => ctx.apply(),
            Phase::Flush => ctx.flush(),
            Phase::Relaunch => ctx.relaunch(),
            Phase::Done => ctx.done(),
        };
    }
}

impl Ctx {
    /// First-run provisioning / repair: the drive carries the launcher but no
    /// component pool, no local manifest, or a wanted component's artifact is
    /// missing → fetch + stage + apply through the regular updater (the same
    /// path as `self-update`), so THIS launch can mount and run.
    fn provision(&mut self) -> Phase {
        if !pool_needs_provision(self.comp_dir.as_ref()) {
            return Phase::Prepare;
        }
        if self.comp_dir.is_none() {
            log("no components on the drive — provisioning from the update server");
        }
        // Determinate splash: a side thread drives the gauge from the updater's
        // download status while the blocking download runs on this thread;
        // apply::run then shows its own gauge for the copy-onto-drive phase.
        if let Some(s) = show_splash(SplashOpts { text: &i18n::t("provisioning"), progress: true }) {
            *self.spinner.lock().unwrap() = Some(s);
        }
        let prog_stop = Arc::new(AtomicBool::new(false));
        let prog = {
            let up = self.updater.clone();
            let sp = self.spinner.clone();
            let stop = prog_stop.clone();
            std::thread::spawn(move || {
                let mut last = u8::MAX;
                while !stop.load(Ordering::Relaxed) {
                    let st = up.status();
                    if st.total > 0 {
                        // Prefer byte-level progress (steady) over file-count
                        // progress (jumps once per file).
                        let pct = if st.total_bytes > 0 {
                            ((st.done_bytes * 100 / st.total_bytes) as u8).min(100)
                        } else {
                            ((st.done * 100 / st.total) as u8).min(100)
                        };
                        if let Ok(mut g) = sp.lock() {
                            if let Some(splash) = g.as_mut() {
                                if pct != last {
                                    splash.set_progress(pct);
                                    last = pct;
                                }
                                // Text carries the throughput indicator (refreshed
                                // each tick so the rate stays live).
                                let mut text = i18n::t_args(
                                    "provisioning-progress",
                                    &[("done", st.done as i64), ("total", st.total as i64)],
                                );
                                if st.rate_bps > 0 {
                                    text.push_str(&format!(" — {}/s", update::human_bytes(st.rate_bps)));
                                }
                                splash.set_text(&text);
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            })
        };
        let provisioned = match tokio::runtime::Runtime::new() {
            Ok(boot_rt) => {
                boot_rt.block_on(update::check_and_predownload(self.updater.clone()));
                prog_stop.store(true, Ordering::Relaxed);
                let _ = prog.join();
                kill_spinner(&self.spinner); // close the download gauge before apply's own
                apply::run(&self.updater) // stages → drive (verified, atomic, commits update.json)
            }
            Err(e) => {
                prog_stop.store(true, Ordering::Relaxed);
                let _ = prog.join();
                log(&format!("bootstrap: runtime: {e}"));
                false
            }
        };
        kill_spinner(&self.spinner);
        if provisioned {
            self.comp_dir = components_dir(&self.here);
        } else {
            log("bootstrap: provisioning did not complete — starting without a component pool");
        }
        Phase::Prepare
    }

    /// Mount the components on the HOST and export the env the stack inherits.
    /// Mounting here means FUSE uses the host's fusermount + /dev/fuse; the FHS
    /// sandbox then sees the mounts through its recursive bind. The FHS child
    /// only resolves its app dir from the inherited PLANAI_RESOURCES.
    fn prepare(&mut self) -> Phase {
        if self.in_fhs {
            if let Some(res) = self.resources.as_ref() {
                let a = res.join("app");
                if a.exists() {
                    self.app_dir = Some(a);
                }
            }
            return Phase::Stack;
        }

        // Splash ASAP — it covers the slow first-run mount/extract below, when
        // no window exists yet. Killed when Electron signals ready (/api/ready),
        // on the post-spawn safety net, or on exit.
        if let Some(splash) = show_splash(SplashOpts { text: &i18n::t("starting-preparing"), progress: false }) {
            *self.spinner.lock().unwrap() = Some(splash);
        }

        // Decide the first-run bootstrap NOW: read_selection() below creates
        // platforms.json, which would erase the "drive has no platforms.json
        // yet" signal this flag is built from.
        self.bootstrap_update = update::load_local().is_none() || !update::platforms_exists();

        if self.resources.is_some() {
            // Something upstream already prepared the resources — don't touch them.
            return self.next_after_prepare();
        }
        let Some(comp) = self.comp_dir.clone() else {
            return self.next_after_prepare();
        };

        // models/ + data/ live on the USB beside the components/ dir. Pin the
        // portable root to the drive root so they resolve to the USB even when
        // the launcher runs from a mounted dmg. Honour an explicit override.
        if std::env::var_os("PLANAI_PORTABLE_ROOT").is_none() {
            if let Some(usb_root) = pool_drive_root(&comp) {
                std::env::set_var("PLANAI_PORTABLE_ROOT", usb_root);
            }
        }
        // Finish any update apply that a prior run left mid-way (before we
        // mount the components it may be replacing).
        apply::resume_if_interrupted();
        let root = cache_root().join("root");
        let dist = root.join("dist");
        let tools = root.join("tools");
        let _ = std::fs::create_dir_all(&dist);
        // FUSE-mount first on every platform — incl. NixOS (we're on the host
        // here, before the FHS sandbox). provide() falls back to unsquashfs
        // extraction if the mount fails; PLANAI_FORCE_EXTRACT forces it.
        let force_extract = std::env::var_os("PLANAI_FORCE_EXTRACT").is_some();
        if force_extract {
            log("PLANAI_FORCE_EXTRACT set — extracting components (no FUSE mount)");
        } else if crate::is_nixos() {
            log("NixOS — FUSE-mounting components on the host (extract fallback)");
        }

        // The python runtime + ow-assets ARE the "openwebui" feature: with it
        // disabled (or the component pruned) we run without Open-WebUI instead
        // of exiting — ollama + the dashboard still work.
        let features = update::read_selection().features;
        let openwebui_on = features.iter().any(|f| f == "openwebui");
        match pick_base(&comp, "runtime-") {
            Some(rt) if openwebui_on => match provide(&comp, &rt, &dist.join("runtime"), &tools, force_extract) {
                Ok(k) => self.mounts.push(Mount { dest: dist.join("runtime"), kind: k }),
                Err(e) => {
                    log(&format!("runtime: {e}"));
                    self.exit_code = 1;
                    return Phase::Teardown;
                }
            },
            Some(_) => log("openwebui feature disabled — skipping the runtime mount"),
            None if openwebui_on => log("no runtime component found — running without Open-WebUI"),
            None => {}
        }
        if openwebui_on
            && (comp.join("ow-assets.squashfs").exists() || comp.join("ow-assets.dmg").exists() || comp.join("ow-assets").is_dir())
        {
            if let Ok(k) = provide(&comp, "ow-assets", &dist.join("ow-assets"), &tools, force_extract) {
                self.mounts.push(Mount { dest: dist.join("ow-assets"), kind: k });
            }
        }
        // hermes: the optional agent component (feature "hermes", default-off).
        if features.iter().any(|f| f == "hermes")
            && (comp.join("hermes.squashfs").exists() || comp.join("hermes.dmg").exists() || comp.join("hermes").is_dir())
        {
            match provide(&comp, "hermes", &dist.join("hermes"), &tools, force_extract) {
                Ok(k) => self.mounts.push(Mount { dest: dist.join("hermes"), kind: k }),
                Err(e) => log(&format!("hermes: {e}")),
            }
        }
        // hermes-webui: the lightweight hermes web UI (same feature) — pure
        // python/static sources run with the hermes component's python.
        if features.iter().any(|f| f == "hermes")
            && (comp.join("hermes-webui.squashfs").exists() || comp.join("hermes-webui.dmg").exists() || comp.join("hermes-webui").is_dir())
        {
            match provide(&comp, "hermes-webui", &dist.join("hermes-webui"), &tools, force_extract) {
                Ok(k) => self.mounts.push(Mount { dest: dist.join("hermes-webui"), kind: k }),
                Err(e) => log(&format!("hermes-webui: {e}")),
            }
        }
        if let Some((ol, why)) = detect_ollama(&comp) {
            log(&format!("ollama flavour: {ol} — {why}"));
            match provide(&comp, &ol, &dist.join("ollama"), &tools, force_extract) {
                Ok(k) => self.mounts.push(Mount { dest: dist.join("ollama"), kind: k }),
                Err(e) => log(&format!("ollama: {e}")),
            }
            std::env::set_var("PLANAI_OLLAMA_FLAVOUR", ol);
            std::env::set_var("PLANAI_OLLAMA_REASON", why);
        }
        // llama.cpp (optional feature): GPU-detected flavour, mounted at a
        // flavour-neutral dist/llamacpp like ollama's dist/ollama.
        if features.iter().any(|f| f == "llamacpp") {
            if let Some((lc, why)) = detect_llamacpp(&comp) {
                log(&format!("llamacpp flavour: {lc} — {why}"));
                match provide(&comp, &lc, &dist.join("llamacpp"), &tools, force_extract) {
                    Ok(k) => self.mounts.push(Mount { dest: dist.join("llamacpp"), kind: k }),
                    Err(e) => log(&format!("llamacpp: {e}")),
                }
                std::env::set_var("PLANAI_LLAMACPP_FLAVOUR", lc);
                std::env::set_var("PLANAI_LLAMACPP_REASON", why);
            }
        }
        // The Electron app itself ships as a component (app-<os>): mount/link it
        // and run Electron from the mounted tree.
        if let Some(app) = pick_base(&comp, "app-") {
            match provide(&comp, &app, &dist.join("app"), &tools, force_extract) {
                Ok(k) => {
                    self.mounts.push(Mount { dest: dist.join("app"), kind: k });
                    self.app_dir = Some(dist.join("app"));
                }
                Err(e) => log(&format!("app: {e}")),
            }
        }
        // The plan.ai USB daemon (usbd → dist/usbd/mac-mgmt[.exe]). Optional —
        // absent in dev / older bundles, where the launcher falls back to its
        // own supervisor.
        if comp.join("usbd.squashfs").exists() || comp.join("usbd.dmg").exists() || comp.join("usbd").is_dir() {
            match provide(&comp, "usbd", &dist.join("usbd"), &tools, force_extract) {
                Ok(k) => self.mounts.push(Mount { dest: dist.join("usbd"), kind: k }),
                Err(e) => log(&format!("usbd: {e}")),
            }
        }
        std::env::set_var("PLANAI_RESOURCES", &dist);
        // Electron's node loader finds the shared pool via this (the app runs
        // from <cache>/dist/app, not from beside the pool).
        std::env::set_var("PLANAI_COMPONENTS", &comp);
        self.resources = Some(dist);
        self.next_after_prepare()
    }

    /// Host: hand the stack to the FHS sandbox on NixOS, else run it ourselves.
    fn next_after_prepare(&self) -> Phase {
        #[cfg(target_os = "linux")]
        {
            // Cheap pre-check; delegate() re-validates and falls back to Stack.
            if crate::is_nixos() && std::env::var_os("PLANAI_DEV").is_none() {
                return Phase::Delegate;
            }
        }
        Phase::Stack
    }

    /// NixOS host: run the stack inside the FHS sandbox as a child and wait.
    /// The child walks Prepare → Stack → Teardown → Done of this same machine;
    /// its sentinel exit code reports an apply request back to us.
    fn delegate(&mut self) -> Phase {
        #[cfg(target_os = "linux")]
        {
            if let Some(code) = crate::maybe_run_in_fhs(&self.exe, self.comp_dir.as_deref(), &self.spinner) {
                if code == APPLY_REQUESTED_EXIT {
                    // The child quit because the user asked to apply the staged
                    // update — the drive work is ours (the pendrive marker +
                    // staging are in place).
                    self.apply_requested.store(true, Ordering::SeqCst);
                    self.exit_code = 0;
                } else {
                    self.exit_code = code;
                }
                return Phase::Teardown;
            }
        }
        // FHS path doesn't apply / can't be set up — run the stack bare.
        Phase::Stack
    }

    /// The session: llmfit + usbd/supervisor + UI server + Electron, until the
    /// app quits (or /api/update/apply kills it to request an apply).
    fn stack(&mut self) -> Phase {
        // Decided in Prepare (host only — the FHS child's flag stays false; the
        // host already bootstraps).
        let bootstrap_update = self.bootstrap_update;

        // llmfit: GPU detection (→ PLANAI_GPU_JSON for Electron) + the
        // model-browser serve API. Best-effort.
        {
            let lf = std::env::var_os("PLANAI_LLMFIT")
                .map(PathBuf::from)
                .filter(|p| p.exists())
                .or_else(|| {
                    self.comp_dir
                        .as_ref()
                        .and_then(|c| prepare_llmfit(c, &cache_root().join("root").join("tools")))
                });
            if let Some(lf) = lf {
                self.llmfit_child = start_llmfit(&lf);
            }
        }

        // Prefer the Electron binary inside the mounted app component; fall
        // back to one sitting beside the launcher (back-compat).
        let program = self
            .app_dir
            .as_deref()
            .and_then(electron_in)
            .or_else(|| electron_target(&self.here));
        let program = match program {
            Some(p) => p,
            None => {
                log(&format!(
                    "could not find the bundled app (no app-<os> component, none beside {})",
                    self.here.display()
                ));
                self.exit_code = 1;
                return Phase::Teardown;
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
                self.exit_code = 1;
                return Phase::Teardown;
            }
        };
        let self_exe = self.exe.clone();
        let socket = self.socket.clone();
        let spinner = self.spinner.clone();
        let updater = self.updater.clone();
        let apply_requested = self.apply_requested.clone();
        let electron = self.electron.clone();
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
        if bootstrap_update {
            log("no update manifest on the drive — bootstrapping from the update server");
            rt.spawn(update::check_and_predownload(self.updater.clone()));
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
        cmd.args(&self.args);

        // Safety net: the mount is done and the control plane is up, so
        // Electron should paint within seconds and POST /api/ready. If that
        // signal never lands, close the spinner so it can't sit over the session.
        {
            let h = self.spinner.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(30));
                kill_spinner(&h);
            });
        }

        // Spawn Electron into the shared handle so /api/update/apply can
        // terminate it (→ this wait returns → Teardown/Apply run with the
        // runtime down). Poll so the server task can take the lock to kill it.
        self.exit_code = match cmd.spawn() {
            Ok(child) => {
                *self.electron.lock().unwrap() = Some(child);
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let mut g = self.electron.lock().unwrap();
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
        };
        Phase::Teardown
    }

    /// Stop everything in dependency order, so nothing is still executing from
    /// a mount when we unmount it (otherwise the unmount hits "device busy" and
    /// falls back to a lazy detach): splash → daemon/supervisor (their services
    /// run from the mounted trees) → llmfit → the component mounts.
    fn teardown(&mut self) -> Phase {
        kill_spinner(&self.spinner);
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
            let _ = c.kill();
            let _ = c.wait();
        }
        crate::teardown(&self.mounts);

        // Apply is drive work — the host's, with the runtime now fully down.
        if !self.in_fhs && self.apply_requested.load(Ordering::SeqCst) {
            Phase::Apply
        } else {
            Phase::Flush
        }
    }

    /// Apply the staged update onto the drive. The in-memory `Pending` exists
    /// when OUR updater staged it (normal path); after a Delegate the child's
    /// updater staged it, so we apply from the pendrive marker instead — the
    /// same crash-resume path a reboot uses.
    fn apply(&mut self) -> Phase {
        self.applied = if self.updater.pending.lock().unwrap().is_some() {
            apply::run(&self.updater)
        } else {
            apply::resume_if_interrupted()
        };
        Phase::Flush
    }

    /// Flush + "safe to unplug" after unmount. The FHS child skips this — its
    /// host parent does it after we exit (avoids a double notification).
    fn flush(&mut self) -> Phase {
        if self.in_fhs {
            return Phase::Done;
        }
        crate::flush_drive();
        if self.applied {
            Phase::Relaunch
        } else {
            Phase::Done
        }
    }

    /// Land the user on the new version. Re-exec the updated binary where
    /// that's meaningful — the binary we're running is the one on the drive the
    /// apply just refreshed — with the ORIGINAL startup environment (the run's
    /// mutations would poison a fresh launch). A launcher running from a
    /// read-only image (the mac dmg mount) still maps the OLD bytes, so
    /// re-running it would relaunch the old version: notify instead.
    fn relaunch(&mut self) -> Phase {
        if !self.exe.starts_with(paths::portable_root()) {
            notify("plan.ai", &i18n::t("update-applied-restart"));
            return Phase::Done;
        }
        // Release the single-instance lock NOW: the relaunched process takes it
        // during its startup, and racing the OS-level release on our exit loses.
        drop(self.instance_lock.take());

        let mut cmd = Command::new(&self.exe);
        cmd.args(&self.args).env_clear().envs(self.env0.iter().map(|(k, v)| (k.clone(), v.clone())));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // exec replaces this process — same PID, same terminal. Only
            // returns on failure.
            let err = cmd.exec();
            log(&format!("relaunch: exec {} failed: {err}", self.exe.display()));
        }
        #[cfg(not(unix))]
        {
            match cmd.spawn() {
                Ok(_) => {
                    log("relaunch: spawned the updated launcher");
                    return Phase::Done;
                }
                Err(e) => log(&format!("relaunch: spawn {} failed: {e}", self.exe.display())),
            }
        }
        notify("plan.ai", &i18n::t("update-applied-restart"));
        Phase::Done
    }

    /// Exit. The FHS child maps an apply request onto the sentinel exit code so
    /// the host (which owns the drive) runs Apply after its own teardown.
    fn done(&mut self) -> ! {
        if self.in_fhs && self.apply_requested.load(Ordering::SeqCst) {
            std::process::exit(APPLY_REQUESTED_EXIT);
        }
        std::process::exit(self.exit_code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The teardown → apply/flush decision: only the HOST applies, and only on
    /// an explicit request (the SPA's apply button, or the child's sentinel).
    #[test]
    fn host_applies_only_on_request_and_child_never_does() {
        let case = |in_fhs: bool, requested: bool| {
            if !in_fhs && requested {
                Phase::Apply
            } else {
                Phase::Flush
            }
        };
        assert_eq!(case(false, true), Phase::Apply);
        assert_eq!(case(false, false), Phase::Flush);
        assert_eq!(case(true, true), Phase::Flush, "the FHS child must never touch the drive");
        assert_eq!(case(true, false), Phase::Flush);
    }

    /// The sentinel must be distinguishable from a plain exit and stable: the
    /// host hard-codes the same constant the child exits with.
    #[test]
    fn apply_sentinel_is_not_a_normal_exit_code() {
        assert_ne!(APPLY_REQUESTED_EXIT, 0);
        assert_ne!(APPLY_REQUESTED_EXIT, 1);
        assert_eq!(APPLY_REQUESTED_EXIT, 75);
    }
}
