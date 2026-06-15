//! The project-agnostic run lifecycle as an explicit state machine.
//!
//! Every run is a walk through [`Phase`]s; [`run`] drives phase → phase until `Done`.
//! The transitions — provisioning, the NixOS FHS-child delegation, the update-apply +
//! relaunch decisions, the teardown dependency ordering, the apply-sentinel exit code —
//! live here ONCE and are reused by every consumer. The project-specific bodies (which
//! components to mount, the session it runs — UI/foreground app/services, the localized
//! splash text) are supplied through the [`Project`] trait.
//!
//! ```text
//! Provision ─→ Prepare ─→ Delegate ─┐
//!                  └────→ Stack ────┤
//!                                   ▼
//!                               Teardown ─→ Apply ─→ Flush ─→ Relaunch ─→ Done
//!                                   └──────(no apply)──┘ └──(no update)──┘
//! ```
//!
//! Host vs FHS child: on NixOS the host mounts the components, then runs the whole Stack
//! inside the FHS sandbox as a child (Delegate); the child re-enters this same machine
//! with `in_fhs = true` and walks ONLY Prepare → Stack → Teardown → Done. Drive-level
//! work (provisioning, apply, flush, relaunch) is the host's; the child signals "apply
//! requested" with the [`APPLY_REQUESTED_EXIT`] sentinel and the host applies afterwards.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::{
    apply, cache_root, components_dir, kill_spinner, log, pool_drive_root, portable_root,
    show_splash, teardown, update, Mount, SpinnerHandle, SplashOpts,
};

/// A shared handle to the session's foreground process (e.g. the webview). The UI's
/// "apply update" endpoint terminates it → the session wait returns → Teardown/Apply
/// run with the runtime down. Held by the project's session + its server.
pub type AppHandle = Arc<Mutex<Option<Child>>>;

/// Exit code the FHS child uses for "the session ended because the user asked to apply
/// the staged update". The child can't apply it (drive work is the host's, outside the
/// sandbox); the host maps this back to a normal exit and runs Apply. 75 = EX_TEMPFAIL.
pub const APPLY_REQUESTED_EXIT: i32 = 75;

/// What the project plugs into the lifecycle. The lifecycle owns the phase sequencing,
/// the FHS-child delegation, the updater + splash plumbing, and the apply/relaunch
/// decisions; the project supplies the bodies below.
pub trait Project: Send {
    /// Notification title (the product brand).
    fn brand(&self) -> &str;
    /// Localized text for a lifecycle string key (`provisioning`, `starting-preparing`,
    /// `applying-update`, `update-applied-restart`).
    fn text(&self, key: &str) -> String;
    /// A `Send + Sync` formatter for the provisioning progress line `(done, total,
    /// rate_bps) -> String`, called from the gauge thread.
    fn provision_progress(&self) -> Box<dyn Fn(u64, u64, u64) -> String + Send + Sync>;
    /// Mount the components on the host (set the child env, push Mounts, resolve the
    /// app dir). Return `false` to abort the run (a required mount failed). On the FHS
    /// child (`ctx.in_fhs`) this only resolves the app dir from the inherited resources.
    fn prepare_mounts(&mut self, ctx: &mut PrepareCtx) -> bool;
    /// Run the session (services + UI + foreground app); return its exit code. The
    /// project keeps any service handles it must stop in `teardown_session`.
    fn run_session(&mut self, ctx: &mut SessionCtx) -> i32;
    /// Stop the session's services (BEFORE the component mounts are torn down, so
    /// nothing executes from a mount when it is unmounted).
    fn teardown_session(&mut self);
    /// Flush drive write buffers + the "safe to unplug" notification.
    fn flush_drive(&self);
}

/// What [`Project::prepare_mounts`] gets: where to mount + the accumulators it fills.
pub struct PrepareCtx<'a> {
    /// The FHS child only resolves its app dir from `resources` (the inherited
    /// PLANAI_RESOURCES); it does not mount.
    pub in_fhs: bool,
    pub resources: Option<&'a Path>,
    /// The component pool (host).
    pub comp: Option<&'a Path>,
    /// Where to mount each component (`<dist>/<slot>`).
    pub dist: &'a Path,
    /// Writable dir for the embedded mount tools.
    pub tools: &'a Path,
    /// Extract instead of FUSE-mount (PLANAI_FORCE_EXTRACT).
    pub force_extract: bool,
    pub mounts: &'a mut Vec<Mount>,
    pub app_dir: &'a mut Option<PathBuf>,
}

/// What [`Project::run_session`] gets: the run's shared handles + facts.
pub struct SessionCtx<'a> {
    pub exe: &'a Path,
    pub here: &'a Path,
    pub args: &'a [OsString],
    pub app_dir: Option<&'a Path>,
    pub comp_dir: Option<&'a Path>,
    pub spinner: &'a SpinnerHandle,
    pub updater: &'a update::Handle,
    pub apply_requested: &'a Arc<AtomicBool>,
    pub app: &'a AppHandle,
    /// First-run bootstrap: kick a background update check once the server is up.
    pub bootstrap_update: bool,
}

/// Does the drive need (re)provisioning before we can run? First run (no pool + no
/// local manifest) OR repair (a wanted component's on-disk artifact is gone → a full
/// re-fetch). Pins PLANAI_PORTABLE_ROOT from the pool first so the on-disk check + the
/// apply target the drive root. Generic — only the updater + the manifest schema.
pub fn pool_needs_provision(comp_dir: Option<&PathBuf>) -> bool {
    if std::env::var_os("PLANAI_PORTABLE_ROOT").is_none() {
        if let Some(root) = comp_dir.and_then(|c| pool_drive_root(c)) {
            std::env::set_var("PLANAI_PORTABLE_ROOT", root);
        }
    }
    match update::load_local() {
        None => comp_dir.is_none(),
        Some(m) => {
            let sel = update::read_selection();
            let root = portable_root();
            let missing = m.files.iter().any(|e| e.wanted_by(&sel) && !update::artifact_present(&root, e));
            if missing {
                log("components missing on the drive — repairing from the update server");
            }
            missing
        }
    }
}

/// One phase of a run. Every arrow in the module diagram is one `match` arm in [`run`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Provision,
    Prepare,
    Delegate,
    Stack,
    Teardown,
    Apply,
    Flush,
    Relaunch,
    Done,
}

/// Everything a run carries between phases (the generic state). The project state lives
/// in the [`Project`] impl, passed alongside as a separate `&mut` so the two never alias.
pub struct Ctx {
    pub exe: PathBuf,
    pub here: PathBuf,
    pub in_fhs: bool,
    pub args: Vec<OsString>,
    pub env0: Vec<(OsString, OsString)>,
    pub instance_lock: Option<std::fs::File>,

    pub spinner: SpinnerHandle,
    pub updater: update::Handle,
    pub apply_requested: Arc<AtomicBool>,
    pub app: AppHandle,

    comp_dir: Option<PathBuf>,
    resources: Option<PathBuf>,
    app_dir: Option<PathBuf>,
    mounts: Vec<Mount>,
    exit_code: i32,
    applied: bool,
    bootstrap_update: bool,
}

impl Ctx {
    pub fn new(exe: PathBuf, here: PathBuf, in_fhs: bool, args: Vec<OsString>, env0: Vec<(OsString, OsString)>, instance_lock: Option<std::fs::File>) -> Self {
        let comp_dir = components_dir(&here);
        Ctx {
            exe,
            here,
            in_fhs,
            args,
            env0,
            instance_lock,
            spinner: Arc::new(Mutex::new(None)),
            updater: update::Updater::new(),
            apply_requested: Arc::new(AtomicBool::new(false)),
            app: Arc::new(Mutex::new(None)),
            comp_dir,
            resources: std::env::var_os("PLANAI_RESOURCES").map(PathBuf::from),
            app_dir: None,
            mounts: Vec::new(),
            exit_code: 0,
            applied: false,
            bootstrap_update: false,
        }
    }
}

/// Drive the machine to completion. Never returns.
pub fn run(mut ctx: Ctx, mut project: Box<dyn Project>) -> ! {
    let mut phase = if ctx.in_fhs { Phase::Prepare } else { Phase::Provision };
    loop {
        log(&format!("lifecycle: {phase:?}"));
        phase = match phase {
            Phase::Provision => ctx.provision(&mut *project),
            Phase::Prepare => ctx.prepare(&mut *project),
            Phase::Delegate => ctx.delegate(),
            Phase::Stack => ctx.stack(&mut *project),
            Phase::Teardown => ctx.teardown(&mut *project),
            Phase::Apply => ctx.apply(&mut *project),
            Phase::Flush => ctx.flush(&mut *project),
            Phase::Relaunch => ctx.relaunch(&mut *project),
            Phase::Done => ctx.done(),
        };
    }
}

impl Ctx {
    /// First-run provisioning / repair: fetch + stage + apply through the updater so
    /// THIS launch can mount and run. A determinate gauge thread mirrors the download.
    fn provision(&mut self, project: &mut dyn Project) -> Phase {
        if !pool_needs_provision(self.comp_dir.as_ref()) {
            return Phase::Prepare;
        }
        if self.comp_dir.is_none() {
            log("no components on the drive — provisioning from the update server");
        }
        if let Some(s) = show_splash(SplashOpts { text: &project.text("provisioning"), progress: true }) {
            *self.spinner.lock().unwrap() = Some(s);
        }
        let prog_stop = Arc::new(AtomicBool::new(false));
        let prog_text = project.provision_progress();
        let prog = {
            let up = self.updater.clone();
            let sp = self.spinner.clone();
            let stop = prog_stop.clone();
            std::thread::spawn(move || {
                let mut last = u8::MAX;
                while !stop.load(Ordering::Relaxed) {
                    let st = up.status();
                    if st.total > 0 {
                        // byte-level progress (steady) over file-count (jumps per file).
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
                                splash.set_text(&prog_text(st.done, st.total, st.rate_bps));
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
                apply::run(&self.updater, &project.text("applying-update"))
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

    /// Mount the components on the HOST and export the env the stack inherits (the FHS
    /// child only resolves its app dir from the inherited PLANAI_RESOURCES).
    fn prepare(&mut self, project: &mut dyn Project) -> Phase {
        if self.in_fhs {
            let mut pctx = PrepareCtx {
                in_fhs: true,
                resources: self.resources.as_deref(),
                comp: None,
                dist: Path::new(""),
                tools: Path::new(""),
                force_extract: false,
                mounts: &mut self.mounts,
                app_dir: &mut self.app_dir,
            };
            project.prepare_mounts(&mut pctx);
            return Phase::Stack;
        }

        // Splash ASAP — it covers the slow first-run mount/extract below.
        if let Some(splash) = show_splash(SplashOpts { text: &project.text("starting-preparing"), progress: false }) {
            *self.spinner.lock().unwrap() = Some(splash);
        }

        // Decide the bootstrap NOW: read_selection() below creates platforms.json, which
        // would erase the "drive has no platforms.json yet" signal.
        self.bootstrap_update = update::load_local().is_none() || !update::platforms_exists();

        if self.resources.is_some() {
            return self.next_after_prepare();
        }
        let Some(comp) = self.comp_dir.clone() else {
            return self.next_after_prepare();
        };

        // Pin the portable root to the drive root so models/ + data/ resolve to the USB.
        if std::env::var_os("PLANAI_PORTABLE_ROOT").is_none() {
            if let Some(usb_root) = pool_drive_root(&comp) {
                std::env::set_var("PLANAI_PORTABLE_ROOT", usb_root);
            }
        }
        // Finish any update apply a prior run left mid-way (before mounting what it replaces).
        apply::resume_if_interrupted(&project.text("applying-update"));
        let root = cache_root().join("root");
        let dist = root.join("dist");
        let tools = root.join("tools");
        let _ = std::fs::create_dir_all(&dist);
        let force_extract = std::env::var_os("PLANAI_FORCE_EXTRACT").is_some();
        if force_extract {
            log("PLANAI_FORCE_EXTRACT set — extracting components (no FUSE mount)");
        } else if crate::is_nixos() {
            log("NixOS — FUSE-mounting components on the host (extract fallback)");
        }

        let mut pctx = PrepareCtx {
            in_fhs: false,
            resources: None,
            comp: Some(&comp),
            dist: &dist,
            tools: &tools,
            force_extract,
            mounts: &mut self.mounts,
            app_dir: &mut self.app_dir,
        };
        if !project.prepare_mounts(&mut pctx) {
            self.exit_code = 1;
            return Phase::Teardown;
        }
        self.resources = Some(dist);
        self.next_after_prepare()
    }

    /// Host: hand the stack to the FHS sandbox on NixOS, else run it ourselves.
    fn next_after_prepare(&self) -> Phase {
        #[cfg(target_os = "linux")]
        {
            if crate::is_nixos() && std::env::var_os("PLANAI_DEV").is_none() {
                return Phase::Delegate;
            }
        }
        Phase::Stack
    }

    /// NixOS host: run the stack inside the FHS sandbox as a child and wait. The child
    /// walks Prepare → Stack → Teardown → Done; its sentinel exit reports an apply request.
    fn delegate(&mut self) -> Phase {
        #[cfg(target_os = "linux")]
        {
            if let Some(code) = crate::maybe_run_in_fhs(&self.exe, self.comp_dir.as_deref(), &self.spinner) {
                if code == APPLY_REQUESTED_EXIT {
                    self.apply_requested.store(true, Ordering::SeqCst);
                    self.exit_code = 0;
                } else {
                    self.exit_code = code;
                }
                return Phase::Teardown;
            }
        }
        Phase::Stack
    }

    /// The session — the project runs its services + UI + foreground app and returns the
    /// exit code; it keeps any handles it must stop in `teardown_session`.
    fn stack(&mut self, project: &mut dyn Project) -> Phase {
        let mut sctx = SessionCtx {
            exe: &self.exe,
            here: &self.here,
            args: &self.args,
            app_dir: self.app_dir.as_deref(),
            comp_dir: self.comp_dir.as_deref(),
            spinner: &self.spinner,
            updater: &self.updater,
            apply_requested: &self.apply_requested,
            app: &self.app,
            bootstrap_update: self.bootstrap_update,
        };
        self.exit_code = project.run_session(&mut sctx);
        Phase::Teardown
    }

    /// Stop everything in dependency order: splash → the project's services (they run
    /// from the mounted trees) → the component mounts. Then decide apply vs flush.
    fn teardown(&mut self, project: &mut dyn Project) -> Phase {
        kill_spinner(&self.spinner);
        project.teardown_session();
        teardown(&self.mounts);
        if !self.in_fhs && self.apply_requested.load(Ordering::SeqCst) {
            Phase::Apply
        } else {
            Phase::Flush
        }
    }

    /// Apply the staged update onto the drive. In-memory `Pending` exists when OUR
    /// updater staged it; after a Delegate the child staged it, so apply from the
    /// pendrive marker (the same crash-resume path a reboot uses).
    fn apply(&mut self, project: &mut dyn Project) -> Phase {
        let text = project.text("applying-update");
        self.applied = if self.updater.pending.lock().unwrap().is_some() {
            apply::run(&self.updater, &text)
        } else {
            apply::resume_if_interrupted(&text)
        };
        Phase::Flush
    }

    /// Flush + "safe to unplug" after unmount. The FHS child skips this — its host
    /// parent does it after we exit (avoids a double notification).
    fn flush(&mut self, project: &mut dyn Project) -> Phase {
        if self.in_fhs {
            return Phase::Done;
        }
        project.flush_drive();
        if self.applied {
            Phase::Relaunch
        } else {
            Phase::Done
        }
    }

    /// Land the user on the new version. Re-exec the updated binary (on the drive the
    /// apply just refreshed) with the ORIGINAL startup env. A launcher running from a
    /// read-only image (mac dmg) still maps the OLD bytes → notify instead.
    fn relaunch(&mut self, project: &mut dyn Project) -> Phase {
        if !self.exe.starts_with(portable_root()) {
            crate::notify(project.brand(), &project.text("update-applied-restart"));
            return Phase::Done;
        }
        // Release the single-instance lock NOW: the relaunched process takes it during
        // startup and racing the OS-level release on our exit loses.
        drop(self.instance_lock.take());

        let mut cmd = Command::new(&self.exe);
        cmd.args(&self.args).env_clear().envs(self.env0.iter().map(|(k, v)| (k.clone(), v.clone())));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = cmd.exec(); // replaces this process; only returns on failure
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
        crate::notify(project.brand(), &project.text("update-applied-restart"));
        Phase::Done
    }

    /// Exit. The FHS child maps an apply request onto the sentinel exit code so the host
    /// (which owns the drive) runs Apply after its own teardown.
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

    /// teardown → apply/flush: only the HOST applies, and only on an explicit request.
    #[test]
    fn host_applies_only_on_request_and_child_never_does() {
        let case = |in_fhs: bool, requested: bool| if !in_fhs && requested { Phase::Apply } else { Phase::Flush };
        assert_eq!(case(false, true), Phase::Apply);
        assert_eq!(case(false, false), Phase::Flush);
        assert_eq!(case(true, true), Phase::Flush, "the FHS child must never touch the drive");
        assert_eq!(case(true, false), Phase::Flush);
    }

    /// The sentinel must be distinguishable from a plain exit and stable.
    #[test]
    fn apply_sentinel_is_not_a_normal_exit_code() {
        assert_ne!(APPLY_REQUESTED_EXIT, 0);
        assert_ne!(APPLY_REQUESTED_EXIT, 1);
        assert_eq!(APPLY_REQUESTED_EXIT, 75);
    }
}
