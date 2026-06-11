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
mod apply;
mod control;
mod i18n;
mod net;
mod paths;
mod proxy;
mod serve;
mod update;
mod usbd;

// Embedded static tools (non-empty only on linux; see build.rs).
const SQUASHFUSE_LL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/squashfuse_ll"));
const UNSQUASHFS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/unsquashfs"));
// Static bubblewrap for the NixOS FHS path (sets up the outer namespace that provides
// the FHS-closure squashfs as /nix/store). Empty off-linux / in dev builds.
const BWRAP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bwrap"));
// Embedded splash spinner for THIS target (all OSes; empty in dev — see build.rs).
const SPINNER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/spinner"));

struct Mount {
    dest: PathBuf,
    kind: MountKind,
}
enum MountKind {
    Fuse,
    Dmg,
    None,
}

pub(crate) fn log(msg: &str) {
    eprintln!("[plan-ai] {msg}");
}

/// Best-effort cross-platform desktop notification (notify-rust: Linux D-Bus,
/// macOS, Windows toast).
fn notify(title: &str, body: &str) {
    let _ = notify_rust::Notification::new().summary(title).body(body).show();
}

/// A running splash — the single shared progress abstraction for BOTH the GUI and
/// the terminal. Every caller uses the same `set_text` / `set_progress` / `close`
/// API regardless of which backend `show_splash` picked:
///   - `Proc`: an external splash process — the embedded CPU-rendered splash window, a GUI
///     progress dialog (zenity/kdialog/yad), or the curses `dialog` gauge — all fed
///     over the zenity-compatible stdin protocol (`#label` / a bare percentage).
///   - `Term`: an in-process progress line drawn on stderr. The universal fallback
///     when no GUI/dialog tool is available but we have a tty, so progress is never
///     silently lost (no external dependency).
pub enum Splash {
    Proc(std::process::Child),
    Term(TermBar),
    /// Last-resort fallback: no GUI window, no dialog, and no tty for a progress line
    /// (e.g. a GUI double-click on a machine where the splash window can't come up).
    /// A one-shot "starting…" desktop notification was already fired when this was
    /// chosen; the progress updates below are intentional no-ops (re-firing toasts per
    /// tick would spam). It still exists as a real handle so callers `close()` it
    /// uniformly.
    Notify,
}

impl Splash {
    /// Update the label.
    pub fn set_text(&mut self, text: &str) {
        match self {
            Splash::Proc(child) => write_proc_line(child, &format!("#{text}")),
            Splash::Term(bar) => bar.set_text(text),
            Splash::Notify => {}
        }
    }
    /// Update the percentage 0..=100 (determinate/progress splashes).
    pub fn set_progress(&mut self, pct: u8) {
        match self {
            Splash::Proc(child) => write_proc_line(child, &pct.min(100).to_string()),
            Splash::Term(bar) => bar.set_progress(pct),
            Splash::Notify => {}
        }
    }
    /// Close + reap the splash.
    pub fn close(self) {
        match self {
            Splash::Proc(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            Splash::Term(bar) => bar.close(),
            Splash::Notify => {}
        }
    }
}

/// Feed one line of the zenity-style protocol to an external splash's stdin.
fn write_proc_line(child: &mut std::process::Child, line: &str) {
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        let _ = writeln!(stdin, "{line}");
        let _ = stdin.flush();
    }
}

/// In-process terminal progress backend: a single `\r`-redrawn line on stderr,
/// determinate (a bar + percentage) or indeterminate (`label …`). No external deps,
/// works on every OS — the terminal half of the `Splash` abstraction.
pub struct TermBar {
    label: String,
    progress: bool,
    pct: u8,
}

impl TermBar {
    const WIDTH: usize = 24;
    fn new(label: String, progress: bool) -> Self {
        let bar = TermBar { label, progress, pct: 0 };
        bar.draw();
        bar
    }
    fn draw(&self) {
        use std::io::Write;
        let mut err = std::io::stderr();
        if self.progress {
            let filled = (self.pct as usize * Self::WIDTH) / 100;
            let bar: String = "█".repeat(filled) + &"░".repeat(Self::WIDTH - filled);
            let _ = write!(err, "\r[plan-ai] {} ▕{}▏ {:>3}%", self.label, bar, self.pct);
        } else {
            let _ = write!(err, "\r[plan-ai] {} …", self.label);
        }
        let _ = err.flush();
    }
    fn set_text(&mut self, text: &str) {
        self.label = text.to_string();
        self.draw();
    }
    fn set_progress(&mut self, pct: u8) {
        self.pct = pct.min(100);
        self.draw();
    }
    fn close(self) {
        use std::io::Write;
        // Finish the redrawn line so later output starts cleanly.
        let _ = writeln!(std::io::stderr());
    }
}

/// Shared handle to the launch-time splash (closed when Electron signals readiness
/// via /api/ready, or on a timeout / on exit). Shared with the HTTP server so the
/// /api/ready handler can reap it.
pub type SpinnerHandle = std::sync::Arc<std::sync::Mutex<Option<Splash>>>;

/// Shared handle to the Electron child so the update-apply endpoint can terminate
/// it (→ main's wait returns → apply runs). Held by main (waits) + the server.
pub type ElectronHandle = std::sync::Arc<std::sync::Mutex<Option<std::process::Child>>>;

/// Close the splash if it's still up. Idempotent (Option::take).
pub fn kill_spinner(h: &SpinnerHandle) {
    if let Ok(mut g) = h.lock() {
        if let Some(splash) = g.take() {
            splash.close();
        }
    }
}

/// After applying an update, land the user on the new version where we can: Linux/
/// Windows spawn the updated launcher detached (same path, new bytes) + exit; macOS
/// just notifies — the plan-ai.dmg was replaced, so the user reopens it.
fn relaunch_after_update(exe: &Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = exe;
        notify("plan.ai", &i18n::t("update-applied"));
    }
    #[cfg(not(target_os = "macos"))]
    {
        notify("plan.ai", &i18n::t("update-applied"));
        let mut cmd = Command::new(exe);
        cmd.args(std::env::args_os().skip(1));
        let _ = cmd.spawn();
    }
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

/// This bundle's target key for the per-platform component group dir
/// (components/<target>/). OS+arch, so two linux arches (linux-x64 / linux-arm64)
/// read distinct groups instead of colliding in one `linux` bucket. Matches
/// crates/manifest current_platform()/classify(), the build's grouping, and
/// platforms.json.
const POOL_TARGET: &str = if cfg!(target_os = "windows") {
    "win-x64"
} else if cfg!(target_os = "macos") {
    "mac-arm64"
} else if cfg!(target_arch = "aarch64") {
    "linux-arm64"
} else {
    "linux-x64"
};

/// Resolve a `components/` root to the actual pool dir this target reads. Components
/// are grouped per target (components/<target>/), so each binary mounts only its own
/// arch's tree and the manifest is declarative; prefer that. Fall back to a flat pool
/// (components/ holding every platform's files) for dev layouts. Returns None if
/// neither layout is present (no manifest.json marker).
fn resolve_pool(base: &Path) -> Option<PathBuf> {
    let group = base.join(POOL_TARGET);
    if group.join("manifest.json").exists() {
        return Some(group);
    }
    if base.join("manifest.json").exists() {
        return Some(base.to_path_buf());
    }
    None
}

/// The drive root (where models/ + data/ live) for a resolved pool dir, which is
/// either <root>/components (flat) or <root>/components/<os> (per-platform group).
/// Climbs to the `components` dir and returns its parent, so both layouts resolve
/// to the same USB root; falls back to the immediate parent if no such ancestor.
fn pool_drive_root(comp: &Path) -> Option<PathBuf> {
    let mut p = Some(comp);
    while let Some(cur) = p {
        if cur.file_name().is_some_and(|n| n == "components") {
            return cur.parent().map(Path::to_path_buf);
        }
        p = cur.parent();
    }
    comp.parent().map(Path::to_path_buf)
}

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

fn components_dir(here: &Path) -> Option<PathBuf> {
    if let Some(c) = std::env::var_os("PLANAI_COMPONENTS") {
        if let Some(p) = resolve_pool(&PathBuf::from(c)) {
            return Some(p);
        }
    }
    for r in external_roots(here) {
        if let Some(p) = resolve_pool(&r.join("components")) {
            return Some(p);
        }
    }
    // macOS: the launcher .app ships inside a dmg (so its exec bit + signature
    // survive the FAT32 USB), so it runs from the read-only dmg volume — the
    // shared pool isn't beside it, it sits at the root of the USB the dmg was
    // opened from. Scan mounted volumes for it.
    #[cfg(target_os = "macos")]
    {
        if let Ok(entries) = fs::read_dir("/Volumes") {
            for e in entries.flatten() {
                if let Some(p) = resolve_pool(&e.path().join("components")) {
                    return Some(p);
                }
            }
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

// On NixOS the generic glibc Electron/ollama can't run (bare nix-ld stub). We ship the
// buildFHSEnv wrapper's closure as a SQUASHFS (its /nix/store paths don't exist on the
// target). Rather than `nix-store --import` it — which a non-trusted NixOS user can't do
// (the daemon refuses unsigned imports), the bug that left this path dead on stock NixOS
// — we squashfuse-mount it and, in an OUTER bubblewrap namespace, provide it as
// /nix/store: an overlay UNION with the host store where unprivileged overlayfs works,
// else a plain bind that REPLACES it (the closure is self-contained, so replace is fine
// and needs only userns). Inside that namespace we run the wrapper, which sets up the
// FHS and execs OURSELF as a CHILD (PLANAI_FHS_REEXEC=1); the components we already
// FUSE-mounted on the host stay visible via --dev-bind / /, and THIS process survives to
// unmount everything after the sandboxed app exits. Returns the child's exit code, or
// None when the FHS path doesn't apply / can't be set up (caller then runs bare).
#[cfg(target_os = "linux")]
fn maybe_run_in_fhs(exe: &Path, comp: Option<&Path>, spinner: &SpinnerHandle) -> Option<i32> {
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
    let squashfs = comp.join("nixos-fhs.squashfs");
    if !squashfs.exists() {
        log("NixOS FHS: helper squashfs missing — generic binaries may not run");
        return None;
    }
    let tools = cache_root().join("root").join("tools");
    let bwrap = ensure_tool(&tools, "bwrap", BWRAP).or_else(|| {
        log("NixOS FHS: no embedded bwrap — running bare");
        None
    })?;
    // Mount (or extract) the FHS-closure store: <store_root>/<hash> == /nix/store/<hash>
    // (mksquashfs put each store path at the squashfs root by its hash-name).
    let store_root = cache_root().join("root").join("dist").join("nixos-fhs");
    detach_stale_mount(&store_root);
    if fs::create_dir_all(&store_root).is_err() {
        return None;
    }
    let mut mounted = false;
    if let Some(sf) = ensure_tool(&tools, "squashfuse_ll", SQUASHFUSE_LL) {
        let mut cmd = Command::new(&sf);
        cmd.arg(&squashfs).arg(&store_root);
        if let Some(fm) = find_fusermount() {
            cmd.env("FUSERMOUNT_PROG", fm);
        }
        mounted = cmd.status().map(|s| s.success()).unwrap_or(false) && is_mountpoint(&store_root);
    }
    if !mounted {
        // FUSE unavailable — extract instead (a plain dir binds/overlays identically).
        match ensure_tool(&tools, "unsquashfs", UNSQUASHFS) {
            Some(us) => {
                log("NixOS FHS: FUSE mount failed — extracting helper store");
                let ok = Command::new(&us)
                    .args(["-f", "-no-progress", "-d"]).arg(&store_root).arg(&squashfs)
                    .status().map(|s| s.success()).unwrap_or(false);
                if !ok {
                    log("NixOS FHS: extract failed — running bare");
                    return None;
                }
            }
            None => return None,
        }
    }

    // Host-side work is done and the sandboxed Electron is about to start — close the
    // splash now. The FHS child runs in a separate PID namespace and this process blocks
    // until it exits, so the spinner can't be reaped later; the sliver until Electron
    // paints is covered by the SPA's pre-hydration loading banner.
    kill_spinner(spinner);
    let mode = fhs_store_mode(&bwrap);
    log(&format!("NixOS FHS: entering sandbox (store provided via {mode})"));
    // Use the exe path captured at startup, NOT a fresh current_exe(): first-run
    // provisioning may have just self-replaced this binary, so /proc/self/exe now
    // reads "<path> (deleted)" — an invalid path the wrapper can't exec. The startup
    // path still resolves to the (freshly placed) launcher.
    let self_exe = exe.to_path_buf();
    let mut cmd = Command::new(&bwrap);
    cmd.args(["--dev-bind", "/", "/"]); // expose host (incl. host /nix/store + the FUSE-mounted components)
    if mode == "overlay" {
        // Union the closure over the host store in ONE overlay mount — both sets of
        // /nix/store/<hash> resolve (needs unprivileged overlayfs).
        cmd.arg("--overlay-src").arg(&store_root)
            .arg("--overlay-src").arg("/nix/store")
            .args(["--ro-overlay", "/nix/store"]);
    } else {
        // Poor-man's union (needs only userns): keep the host store (already exposed by
        // --dev-bind / /) and bind each closure path ON TOP. Unlike replacing /nix/store,
        // this keeps host paths that inherited env points at (FONTCONFIG_FILE,
        // LOCALE_ARCHIVE, GDK_PIXBUF_MODULE_FILE, …) resolvable — replacing them crashed
        // the renderer (skia/fontconfig). Hash-named dirs never collide.
        if let Ok(rd) = fs::read_dir(&store_root) {
            let mut names: Vec<_> = rd.flatten().map(|e| e.file_name()).collect();
            names.sort();
            for name in names {
                cmd.arg("--ro-bind").arg(store_root.join(&name)).arg(Path::new("/nix/store").join(&name));
            }
        }
    }
    cmd.arg("--").arg(&wrapper).arg(&self_exe).args(std::env::args_os().skip(1))
        .env("PLANAI_FHS_REEXEC", "1");
    let code = match cmd.status() {
        Ok(s) => Some(s.code().unwrap_or(0)),
        Err(e) => {
            log(&format!("NixOS FHS: bwrap spawn failed: {e} — running bare"));
            None
        }
    };
    if mounted {
        let fm = find_fusermount().unwrap_or_else(|| "fusermount".into());
        let _ = Command::new(&fm).arg("-u").arg(&store_root).status();
    }
    code
}

/// Probe unprivileged overlayfs via the embedded bwrap (a throwaway `--ro-overlay`),
/// returning "overlay" if it works, else "bind". Overlay unions the FHS closure with
/// the host store (keeps host paths visible); bind replaces /nix/store with just the
/// closure and needs only userns — the portable default where overlayfs is restricted.
#[cfg(target_os = "linux")]
fn fhs_store_mode(bwrap: &Path) -> &'static str {
    let probe = cache_root().join("ovl-probe");
    let low = probe.join("low");
    let low2 = probe.join("low2");
    let _ = fs::create_dir_all(&low);
    let _ = fs::create_dir_all(&low2);
    let _ = fs::write(low.join("marker"), b"ok");
    // `--ro-overlay` requires at least TWO `--overlay-src` layers — and the real
    // usage unions the FHS-closure store with the host /nix/store (two srcs).
    // Probing with a SINGLE src always errors ("--ro-overlay requires at least
    // two --overlay-src") and wrongly reports "bind" — which then can't create
    // mountpoints on the read-only host /nix/store ("Can't mkdir … Read-only file
    // system"). Probe with two layers so it matches reality.
    let ok = Command::new(bwrap)
        .args(["--ro-bind", "/", "/", "--overlay-src"])
        .arg(&low)
        .arg("--overlay-src")
        .arg(&low2)
        .args(["--ro-overlay", "/mnt", "cat", "/mnt/marker"])
        .output()
        .map(|o| o.status.success() && o.stdout == b"ok")
        .unwrap_or(false);
    let _ = fs::remove_dir_all(&probe);
    if ok { "overlay" } else { "bind" }
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

pub(crate) fn cache_root() -> PathBuf {
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

/// A prior run that crashed or was force-quit before teardown (the supervising
/// test harness `kill`s the launcher; a user can Force-Quit it too) leaves the
/// component still mounted at its FIXED cache `dest`. hdiutil / FUSE then refuse to
/// mount onto the busy mountpoint — hdiutil reports a bare "Permission denied", and
/// the dmg path has no extract fallback, so every subsequent launch would be wedged
/// forever. Best-effort: if `dest` is already a mountpoint, tear that stale mount
/// down so the fresh mount below can take it. No-op when `dest` isn't a mountpoint
/// (the common first-run case) and on windows (its components are junctions, not mounts).
#[cfg(unix)]
fn detach_stale_mount(dest: &Path) {
    if !is_mountpoint(dest) {
        return;
    }
    log(&format!("{}: stale mount from a prior run — detaching before remount", dest.display()));
    #[cfg(target_os = "macos")]
    {
        if !Command::new("hdiutil").arg("detach").arg(dest).status().map(|s| s.success()).unwrap_or(false) {
            let _ = Command::new("hdiutil").arg("detach").arg("-force").arg(dest).status();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let fm = find_fusermount().unwrap_or_else(|| "fusermount".into());
        if !Command::new(&fm).arg("-u").arg(dest).status().map(|s| s.success()).unwrap_or(false) {
            // lazy-detach: a child mid-exit may still reference it; frees on last close.
            let _ = Command::new(&fm).arg("-uz").arg(dest).status();
        }
    }
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
    // Self-heal a mountpoint leaked by a crashed/force-quit prior run before we try
    // to mount onto it (else hdiutil/FUSE fail on the busy dest — "Permission denied").
    #[cfg(unix)]
    detach_stale_mount(dest);
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
    notify("plan.ai", &i18n::t("safe-to-unplug"));
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

/// Is `bin` an executable on $PATH? (`which`, no spawning.)
#[cfg(target_os = "linux")]
fn in_path(bin: &str) -> bool {
    which::which(bin).is_ok()
}

/// Options for the splash window.
#[derive(Clone, Copy)]
pub struct SplashOpts<'a> {
    pub text: &'a str,
    /// Determinate progress-bar mode (fed percentages over stdin); else a spinner.
    pub progress: bool,
}

/// NixOS / no-GL fallback splash: a progress dialog via the desktop's own tool
/// (no GL/glibc deps to ship). zenity (GTK), kdialog (KDE) and yad keep the window
/// for the life of the process, so killing the child closes it — same contract as
/// the splash window. With no display at all but a terminal, the curses `dialog`
/// gauge is the last resort (e.g. a headless SSH / TTY launch). Determinate mode
/// reads 0..100 percentages on stdin (zenity/yad/dialog natively; kdialog stays
/// indeterminate). None if no usable tool is present.
#[cfg(target_os = "linux")]
fn spawn_system_progress_dialog(opts: SplashOpts) -> Option<std::process::Child> {
    use std::io::IsTerminal;
    use std::process::Stdio;
    let title = "plan.ai";
    let text = if opts.text.is_empty() { i18n::t("starting-preparing") } else { opts.text.to_string() };
    // GUI tools need a display; without one they'd spawn then die. Gate them so the
    // terminal `dialog` fallback below is reached on a no-X / headless-TTY launch.
    let have_display = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|k| std::env::var_os(k).map(|v| !v.is_empty()).unwrap_or(false));
    if have_display && in_path("zenity") {
        let mut c = Command::new("zenity");
        c.args(["--progress", "--no-cancel", "--auto-close", "--width=360"]);
        if !opts.progress {
            c.arg("--pulsate"); // indeterminate
        }
        return c
            .arg(format!("--title={title}"))
            .arg(format!("--text={text}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
    }
    if have_display && in_path("kdialog") {
        // kdialog progress is driven over D-Bus; keep it indeterminate (kill to close).
        return Command::new("kdialog")
            .arg(format!("--title={title}"))
            .args(["--progressbar", &text, "0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
    }
    if have_display && in_path("yad") {
        let mut c = Command::new("yad");
        c.args(["--progress", "--no-buttons", "--auto-close"]);
        if !opts.progress {
            c.arg("--pulsate");
        }
        return c
            .arg(format!("--title={title}"))
            .arg(format!("--text={text}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
    }
    // Terminal fallback: the curses `dialog` gauge (needs no X, only a tty). It reads
    // bare 0..100 percentages on stdin — the same protocol Splash::set_progress emits
    // (zenity-compatible); `#label` lines from set_text are non-numeric and ignored.
    // stdout/stderr inherit the tty so curses can draw; killing the child closes it.
    if std::io::stderr().is_terminal() && in_path("dialog") {
        return Command::new("dialog")
            .args(["--title", title, "--gauge", &text, "8", "70", "0"])
            .stdin(Stdio::piped())
            .spawn()
            .ok();
    }
    log("no system dialog (zenity/kdialog/yad/dialog) found — no splash");
    None
}

/// Write the embedded splash spinner (or a PLANAI_SPINNER dev override) to the
/// writable tools dir + chmod +x (FAT32 has no exec bit) and return its path. None if
/// there's no embedded binary (dev/bare-cargo) and no override. Shared by the selftest
/// probe and the real spawn so both run the exact same binary.
fn materialize_spinner() -> Option<PathBuf> {
    let tools = cache_root().join("root").join("tools");
    let _ = fs::create_dir_all(&tools);
    let name = if cfg!(target_os = "windows") { "plan-ai-spinner.exe" } else { "plan-ai-spinner" };
    let bin = tools.join(name);
    let written = if !SPINNER_BIN.is_empty() {
        match fs::write(&bin, SPINNER_BIN) {
            Ok(()) => true,
            Err(e) => { log(&format!("spinner: write {} failed: {e}", bin.display())); false }
        }
    } else if let Some(dev) = std::env::var_os("PLANAI_SPINNER").map(PathBuf::from).filter(|p| p.exists()) {
        match fs::copy(&dev, &bin) {
            Ok(_) => true,
            Err(e) => { log(&format!("spinner: copy dev override failed: {e}")); false }
        }
    } else {
        // The usual reason the splash is absent: a dev/bare-cargo launcher
        // built without PLANAI_SPINNER_BIN, and no PLANAI_SPINNER override set.
        log("spinner: no embedded binary (built without PLANAI_SPINNER_BIN) and no PLANAI_SPINNER override");
        false
    };
    if !written {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&bin, fs::Permissions::from_mode(0o755));
    }
    Some(bin)
}

/// Probe ONCE whether the embedded splash window can actually come up in THIS
/// environment, then cache the verdict. Runs the spinner with `--selftest`: it opens
/// the window, renders a few frames, and exits 0 — or non-zero if the window/GL can't
/// be created (headless / no usable display, a dyld/loader error, …). This is
/// far more reliable than watching the real spinner for an early exit: GL init can
/// take longer than any fixed grace period, and the selftest gives a definitive yes/no
/// before we commit to (or fall back from) the GUI splash. Self-caps at ~5s; we add a
/// hard kill at 8s so a wedged window can't stall startup.
fn splash_renders_here() -> bool {
    use std::process::Stdio;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let Some(bin) = materialize_spinner() else { return false };
        let mut cmd = Command::new(&bin);
        cmd.arg("--selftest").stdin(Stdio::null()).stdout(Stdio::null());
        if std::env::var_os("PLANAI_SPINNER_DEBUG").is_some() {
            cmd.stderr(Stdio::inherit());
        } else {
            cmd.stderr(Stdio::null());
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => { log(&format!("spinner: selftest spawn failed: {e}")); return false; }
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let ok = status.success();
                    if ok {
                        log("spinner: selftest passed — GUI splash usable here");
                    } else {
                        log(&format!("spinner: selftest failed ({status}) — falling back (set PLANAI_SPINNER_DEBUG to see why)"));
                    }
                    return ok;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        log("spinner: selftest timed out — falling back");
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => { log(&format!("spinner: selftest wait failed: {e}")); return false; }
            }
        }
    })
}

/// Spawn the embedded splash window in the chosen mode. Callers must have confirmed
/// it can run via `splash_renders_here()` first. None if it can't be materialized or
/// spawn fails.
fn spawn_splash_window(opts: SplashOpts) -> Option<std::process::Child> {
    use std::process::Stdio;
    let bin = materialize_spinner()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("--text").arg(opts.text);
    if opts.progress {
        cmd.arg("--progress").stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::null());
    // Hide the spinner's own stderr by default, but surface it for diagnosis when
    // PLANAI_SPINNER_DEBUG is set — that's where a no-display / render-init crash
    // prints, which is otherwise invisible (the launcher only sees a dead child).
    if std::env::var_os("PLANAI_SPINNER_DEBUG").is_some() {
        cmd.stderr(Stdio::inherit());
    } else {
        cmd.stderr(Stdio::null());
    }
    match cmd.spawn() {
        Ok(child) => Some(child),
        Err(e) => { log(&format!("spinner: spawn {} failed: {e}", bin.display())); None }
    }
}

/// Show the splash — the one entry point every progress site uses. Picks the best
/// available backend, all behind the same `Splash` API: the embedded splash window
/// where it can run; the desktop's progress dialog on NixOS (can't run the dynamic
/// dynamic binary) or if the splash won't spawn on linux; and finally an in-process terminal
/// progress line whenever we have a tty but no GUI/dialog. Returns None only when
/// truly headless (no display, no tty — e.g. CI). Best-effort.
pub(crate) fn show_splash(opts: SplashOpts) -> Option<Splash> {
    // The GUI splash can still fail across environments: it needs a usable display and
    // a binary that loads cleanly (e.g. NixOS can't run the dynamic binary at all).
    // Rather than guess, ask it directly via a cached `--selftest`
    // probe; only spawn the real window when that proves it works HERE. Otherwise fall
    // back. NixOS short-circuits to the dialog/terminal (the dynamic glibc binary
    // can't run on its bare nix-ld stub at all), so we don't even probe there.
    #[cfg(target_os = "linux")]
    if is_nixos() {
        return Some(fallback_splash(opts));
    }
    if splash_renders_here() {
        if let Some(child) = spawn_splash_window(opts) {
            log("splash spinner shown");
            return Some(Splash::Proc(child));
        }
    }
    // GUI splash unusable here — guaranteed fallback on every platform.
    Some(fallback_splash(opts))
}

/// The non-GUI splash, tried in order and ALWAYS yielding something so progress is
/// never silently lost: a desktop progress dialog (Linux only — zenity/kdialog/yad,
/// or the curses `dialog` on a bare tty); else an in-process terminal progress line
/// (any OS with a tty — incl. the Windows console-subsystem launcher); else a one-shot
/// "starting…" desktop notification (no GUI, no dialog, no tty — e.g. a GUI
/// double-click on a machine with no usable display). Never returns "nothing".
fn fallback_splash(opts: SplashOpts) -> Splash {
    #[cfg(target_os = "linux")]
    if let Some(child) = spawn_system_progress_dialog(opts) {
        return Splash::Proc(child);
    }
    if let Some(s) = term_splash(opts) {
        return s;
    }
    let text = if opts.text.is_empty() { i18n::t("starting-preparing") } else { opts.text.to_string() };
    notify("plan.ai", &text);
    log("splash: no GUI/dialog/tty available — showed a desktop notification");
    Splash::Notify
}

/// The terminal backend of `Splash`: only when stderr is a real tty (otherwise the
/// per-step `log()` lines already narrate progress on a non-interactive stream).
fn term_splash(opts: SplashOpts) -> Option<Splash> {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        log("splash: terminal progress line (no GUI/dialog available)");
        return Some(Splash::Term(TermBar::new(opts.text.to_string(), opts.progress)));
    }
    None
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
    apply::resume_if_interrupted();
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let up = update::Updater::new();
    rt.block_on(update::check_and_predownload(up.clone()));
    let st = up.status();
    use plan_ai_control_api::UpdateState;
    log(&format!("self-update: state={:?} {}/{}", st.state, st.done, st.total));
    if st.state == UpdateState::Ready {
        let applied = apply::run(&up);
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
fn running_ollama_port() -> u16 {
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
        "mac-mgmt" => usbd::resolve_bin(),
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
            // Ops/test: check the update server, pre-download the delta, and apply it
            // (no Electron/runtime). Honours PLANAI_PORTABLE_ROOT + PLANAI_CACHE.
            Some("self-update") => run_self_update(),
            // Passthrough to the bundled tools while the app is running, e.g.
            //   ./plan-ai ollama pull llama3      ./plan-ai mac-mgmt status
            // Resolves the binary from the mounted component tree and forwards the
            // rest of argv + stdio (ollama also gets OLLAMA_HOST/OLLAMA_MODELS for
            // the running server). Never returns.
            Some("ollama") => run_tool_passthrough("ollama", a.collect()),
            Some("mac-mgmt") => run_tool_passthrough("mac-mgmt", a.collect()),
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
    let mut comp_dir = components_dir(&here);

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

    let spinner: SpinnerHandle = std::sync::Arc::new(std::sync::Mutex::new(None));

    // Auto-updater state (manual check from the SPA; bootstrap if the drive has no
    // manifest/platforms). apply_requested + the Electron handle let /api/update/apply
    // quit Electron so this process applies the staged update with the runtime down.
    let updater = update::Updater::new();

    // First-run provisioning: the drive carries the launcher but no component pool and
    // no local manifest → fetch the manifest from the hardcoded update server, stage
    // this platform's components, and apply them onto the USB — all through the regular
    // updater (check_and_predownload → apply::run, the same path as `self-update`), so
    // THIS launch can mount + run instead of erroring out with nothing to start. Once
    // the manifest lands we re-resolve the pool. Skipped in the FHS child (the host
    // already provisioned before re-exec).
    if !in_fhs && pool_needs_provision(comp_dir.as_ref()) {
        if comp_dir.is_none() {
            log("no components on the drive — provisioning from the update server");
        }
        // Determinate splash: a side thread drives the gauge from the updater's download
        // status (file N of M) while the blocking download runs on this thread; apply::run
        // then shows its own gauge for the copy-onto-drive phase. So provisioning shows
        // real progress across both phases instead of an indeterminate spinner.
        if let Some(s) = show_splash(SplashOpts { text: &i18n::t("provisioning"), progress: true }) {
            *spinner.lock().unwrap() = Some(s);
        }
        let prog_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let prog = {
            let up = updater.clone();
            let sp = spinner.clone();
            let stop = prog_stop.clone();
            std::thread::spawn(move || {
                let mut last = u8::MAX;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let st = up.status();
                    if st.total > 0 {
                        // Prefer byte-level progress (steady) over file-count progress
                        // (jumps once per file); fall back to file count pre-download.
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
                                // Text carries the throughput indicator (refreshed each
                                // tick even when pct hasn't moved, so the rate stays live).
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
                boot_rt.block_on(update::check_and_predownload(updater.clone()));
                prog_stop.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = prog.join();
                kill_spinner(&spinner); // close the download gauge before apply's own
                apply::run(&updater) // stages → drive (verified, atomic, commits update.json)
            }
            Err(e) => {
                prog_stop.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = prog.join();
                log(&format!("bootstrap: runtime: {e}"));
                false
            }
        };
        kill_spinner(&spinner);
        if provisioned {
            comp_dir = components_dir(&here);
        } else {
            log("bootstrap: provisioning did not complete — starting without a component pool");
        }
    }

    // Show the splash spinner ASAP (host only) — it covers the slow first-run mount/
    // extract below, when no window exists yet. Killed when Electron signals ready
    // (POST /api/ready), on a timeout, or on exit. The FHS child never spawns one
    // (its PID namespace can't reach the host's; the host closes it before reexec).
    if !in_fhs {
        if let Some(splash) = show_splash(SplashOpts { text: &i18n::t("starting-preparing"), progress: false }) {
            *spinner.lock().unwrap() = Some(splash);
        }
    }
    let apply_requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let electron: ElectronHandle = std::sync::Arc::new(std::sync::Mutex::new(None));
    // Whether to auto-run the download routine this launch (no manifest yet, or
    // platforms.json absent → first-run bootstrap). Computed before read_platforms
    // creates platforms.json.
    let bootstrap_update = !in_fhs && (update::load_local().is_none() || !update::platforms_exists());

    // Prepare components on the HOST (the FHS child skips this — it inherits
    // PLANAI_RESOURCES). Mounting here means FUSE uses the host's fusermount +
    // /dev/fuse; the FHS sandbox then sees the mounts through its recursive bind,
    // so NixOS gets a real mount instead of a slow extraction.
    if !in_fhs && resources.is_none() {
        if let Some(comp) = comp_dir.as_ref() {
            // models/ + data/ live on the USB beside the components/ dir. Pin the
            // portable root to the drive root so they resolve to the USB even when
            // the launcher runs from a mounted dmg (where current_exe's parent is
            // the read-only dmg volume, not the USB). `comp` may be the flat pool
            // (components/) OR a per-OS group (components/<os>/), so climb to the
            // dir that contains `components`, not just comp.parent(). Honour an
            // explicit override.
            if std::env::var_os("PLANAI_PORTABLE_ROOT").is_none() {
                if let Some(usb_root) = pool_drive_root(comp) {
                    std::env::set_var("PLANAI_PORTABLE_ROOT", usb_root);
                }
            }
            // Finish any update apply that a prior run was interrupted mid-way (before
            // we mount the components it may be replacing).
            apply::resume_if_interrupted();
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

            // The python runtime + ow-assets ARE the "openwebui" feature: with it
            // disabled (or the component pruned) we run without Open-WebUI instead
            // of exiting — ollama + the dashboard still work.
            let features = update::read_selection().features;
            let openwebui_on = features.iter().any(|f| f == "openwebui");
            match pick_base(comp, "runtime-") {
                Some(rt) if openwebui_on => match provide(comp, &rt, &dist.join("runtime"), &tools, force_extract) {
                    Ok(k) => mounts.push(Mount { dest: dist.join("runtime"), kind: k }),
                    Err(e) => { log(&format!("runtime: {e}")); std::process::exit(1); }
                },
                Some(_) => log("openwebui feature disabled — skipping the runtime mount"),
                None if openwebui_on => log("no runtime component found — running without Open-WebUI"),
                None => {}
            }
            if openwebui_on
                && (comp.join("ow-assets.squashfs").exists() || comp.join("ow-assets.dmg").exists() || comp.join("ow-assets").is_dir())
            {
                if let Ok(k) = provide(comp, "ow-assets", &dist.join("ow-assets"), &tools, force_extract) {
                    mounts.push(Mount { dest: dist.join("ow-assets"), kind: k });
                }
            }
            // hermes: the optional agent component (feature "hermes", default-off).
            if features.iter().any(|f| f == "hermes")
                && (comp.join("hermes.squashfs").exists() || comp.join("hermes.dmg").exists() || comp.join("hermes").is_dir())
            {
                match provide(comp, "hermes", &dist.join("hermes"), &tools, force_extract) {
                    Ok(k) => mounts.push(Mount { dest: dist.join("hermes"), kind: k }),
                    Err(e) => log(&format!("hermes: {e}")),
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
            // llama.cpp (optional feature): GPU-detected flavour, mounted at a
            // flavour-neutral dist/llamacpp like ollama's dist/ollama.
            if features.iter().any(|f| f == "llamacpp") {
                if let Some((lc, why)) = detect_llamacpp(comp) {
                    log(&format!("llamacpp flavour: {lc} — {why}"));
                    match provide(comp, &lc, &dist.join("llamacpp"), &tools, force_extract) {
                        Ok(k) => mounts.push(Mount { dest: dist.join("llamacpp"), kind: k }),
                        Err(e) => log(&format!("llamacpp: {e}")),
                    }
                    std::env::set_var("PLANAI_LLAMACPP_FLAVOUR", lc);
                    std::env::set_var("PLANAI_LLAMACPP_REASON", why);
                }
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
            // The plan.ai USB daemon (usbd → dist/usbd/mac-mgmt[.exe]): the launcher
            // spawns it as the control plane on every platform (usbd::resolve_bin
            // finds it under PLANAI_RESOURCES). Ships as squashfs (linux) / dmg
            // (mac) / dir (win). Optional — absent in dev / older bundles, where the
            // launcher falls back to its own supervisor (kept as dead-code path).
            if comp.join("usbd.squashfs").exists() || comp.join("usbd.dmg").exists() || comp.join("usbd").is_dir() {
                match provide(comp, "usbd", &dist.join("usbd"), &tools, force_extract) {
                    Ok(k) => mounts.push(Mount { dest: dist.join("usbd"), kind: k }),
                    Err(e) => log(&format!("usbd: {e}")),
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
        // maybe_run_in_fhs closes the NixOS progress dialog itself, right before it
        // launches the sandboxed Electron (the FHS child can't reach it afterwards).
        if let Some(code) = maybe_run_in_fhs(&exe, comp_dir.as_deref(), &spinner) {
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

    // Rust control plane: prefer the plan.ai USB daemon (`mac-mgmt usbd`) when a
    // usbd component is shipped — it owns the mac-mgmt-services supervisor + the
    // spawn-from-mount services AND adds heartbeat / probe / relay / config-sync.
    // The launcher then connects to the daemon's supervisor socket for status and
    // proxies /api/config* to its control port (PLANAI_USBD_URL). Without a usbd
    // binary (plain cargo/dev), fall back to the launcher's own supervisor.
    // Electron is a thin webview that loads PLANAI_UI_URL.
    //
    // Spawn the daemon BEFORE the runtime (single-threaded) so the env it
    // inherits (PLANAI_*) is set safely.
    let usbd_started = usbd::resolve_bin().and_then(|bin| usbd::spawn(&bin));

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => { log(&format!("runtime: {e}")); teardown(&mounts); std::process::exit(1); }
    };
    let socket = supervisor_socket_path();
    let self_exe = exe.clone();
    rt.block_on(async {
        // Daemon mode: connect to the daemon's in-process supervisor socket.
        // Dev mode: start the launcher's own supervisor + register services.
        let client = match &usbd_started {
            Some((_home, sock, _child)) => {
                match mac_mgmt_services::Client::connect(sock, std::time::Duration::from_secs(30)).await {
                    Ok(c) => { log("control: usb daemon owns the supervisor"); Ok(c) }
                    Err(e) => Err(format!("usb daemon socket: {e}")),
                }
            }
            None => control::start_stack(&self_exe, &socket).await.map_err(|e| e.to_string()),
        };
        match client {
            Ok(client) => {
                let port: u16 = std::env::var("PLANAI_UI_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8088);
                match serve::run_server(client, port, spinner.clone(), updater.clone(), apply_requested.clone(), electron.clone()).await {
                    Ok(url) => { std::env::set_var("PLANAI_UI_URL", &url); log(&format!("UI server on {url}")); }
                    Err(e) => log(&format!("serve: {e} — UI may be unavailable")),
                }
            }
            Err(e) => log(&format!("control plane: {e} — services may be unavailable")),
        }
    });

    // First-run bootstrap: no local manifest / no platforms.json → fetch the manifest
    // from the default URL and pre-download the components in the background. Routine
    // update checks are manual (the SPA's "Check for updates").
    if bootstrap_update {
        log("no update manifest on the drive — bootstrapping from the update server");
        rt.spawn(update::check_and_predownload(updater.clone()));
    }

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

    // Safety net: the mount is done and the control plane is up, so Electron should
    // paint within seconds and POST /api/ready. If that signal never lands (renderer
    // error, old build), close the spinner anyway so it can't sit over the session.
    {
        let h = spinner.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(30));
            kill_spinner(&h);
        });
    }

    // Spawn Electron into the shared handle so /api/update/apply can terminate it
    // (→ this wait returns → we apply the staged update). Then wait for it to exit
    // (poll so the server task can take the lock to kill it).
    let code = match cmd.spawn() {
        Ok(child) => {
            *electron.lock().unwrap() = Some(child);
            loop {
                std::thread::sleep(std::time::Duration::from_millis(200));
                let mut g = electron.lock().unwrap();
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

    // Final cleanup: close the splash spinner if it somehow outlived the session
    // (e.g. Electron exited before signalling), then stop the control plane + its
    // managed services, the llmfit serve, and the component mounts — IN THAT ORDER,
    // so nothing is still executing from a mount when we unmount it (otherwise the
    // unmount hits "device busy" and we fall back to a lazy detach).
    kill_spinner(&spinner);
    match usbd_started {
        // usbd mode: the daemon owns the supervisor + spawn-from-mount services.
        // SIGTERM it and wait — it stops its services (which run from the mounted
        // runtime/ollama trees) and exits, releasing every reference into the pool.
        Some((_home, _sock, child)) => usbd::stop(child),
        // dev mode: the launcher's own in-process supervisor — stop it via its socket.
        None => rt.block_on(async {
            if let Ok(mut c) = mac_mgmt_services::Client::connect(&socket, std::time::Duration::from_secs(5)).await {
                let _ = c.shutdown().await;
            }
        }),
    }
    if let Some(mut c) = llmfit_child {
        let _ = c.kill();
        let _ = c.wait();
    }
    teardown(&mounts);

    // Apply a staged update now that the whole runtime is down (Electron + supervisor
    // + mounts gone). Then relaunch the (updated) launcher where we can.
    let applied = if !in_fhs && apply_requested.load(std::sync::atomic::Ordering::SeqCst) {
        apply::run(&updater)
    } else {
        false
    };

    // Flush + "safe to unplug" after unmount. Skip in the in-FHS child — its host
    // parent does the teardown+flush after we exit (avoids a double notification).
    if !in_fhs {
        flush_drive();
    }
    if applied {
        relaunch_after_update(&exe);
    }
    std::process::exit(code);
}
