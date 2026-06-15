//! The project-agnostic loader runtime substrate: logging/notify, the splash
//! abstraction (GUI window / desktop dialog / terminal bar / notification), the
//! component-mount machinery (squashfs FUSE-mount + extract, dmg attach, windows
//! dir-in-place), the NixOS FHS-entry (squashfuse-mount the closure + bubblewrap
//! re-exec), the component-pool discovery, the cache root + single-instance lock.
//!
//! Extracted verbatim from the plan.ai launcher's main.rs. The consuming launcher
//! re-exports these (`pub(crate) use loader_core::*`) and its lifecycle state machine
//! drives them; project-specific concerns (electron, control plane, UI server, GPU
//! flavour detection, update/apply, i18n splash text) stay in the launcher.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// Embedded static tools (non-empty only where applicable; see build.rs).
pub const SQUASHFUSE_LL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/squashfuse_ll"));
pub const UNSQUASHFS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/unsquashfs"));
pub const BWRAP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bwrap"));
pub const SPINNER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/spinner"));

pub struct Mount {
    pub dest: PathBuf,
    pub kind: MountKind,
}
pub enum MountKind {
    Fuse,
    Dmg,
    None,
}

pub fn log(msg: &str) {
    eprintln!("[plan-ai] {msg}");
}

/// Best-effort cross-platform desktop notification (notify-rust).
pub fn notify(title: &str, body: &str) {
    let _ = notify_rust::Notification::new().summary(title).body(body).show();
}

/// A running splash — the single shared progress abstraction for GUI + terminal.
pub enum Splash {
    Proc(std::process::Child),
    Term(TermBar),
    Notify,
}

impl Splash {
    pub fn set_text(&mut self, text: &str) {
        match self {
            Splash::Proc(child) => write_proc_line(child, &format!("#{text}")),
            Splash::Term(bar) => bar.set_text(text),
            Splash::Notify => {}
        }
    }
    pub fn set_progress(&mut self, pct: u8) {
        match self {
            Splash::Proc(child) => write_proc_line(child, &pct.min(100).to_string()),
            Splash::Term(bar) => bar.set_progress(pct),
            Splash::Notify => {}
        }
    }
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

fn write_proc_line(child: &mut std::process::Child, line: &str) {
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        let _ = writeln!(stdin, "{line}");
        let _ = stdin.flush();
    }
}

/// In-process terminal progress backend (a single `\r`-redrawn line on stderr).
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
        let _ = writeln!(std::io::stderr());
    }
}

/// Shared handle to the launch-time splash (closed on /api/ready, timeout, or exit).
pub type SpinnerHandle = std::sync::Arc<std::sync::Mutex<Option<Splash>>>;

/// Close the splash if it's still up. Idempotent (Option::take).
pub fn kill_spinner(h: &SpinnerHandle) {
    if let Ok(mut g) = h.lock() {
        if let Some(splash) = g.take() {
            splash.close();
        }
    }
}

/// Roots beside the launcher where the shared components/ pool lives.
pub fn external_roots(here: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(ai) = std::env::var_os("APPIMAGE") {
        if let Some(d) = Path::new(&ai).parent() {
            roots.push(d.to_path_buf());
        }
    }
    #[cfg(target_os = "macos")]
    {
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

/// This bundle's target key for the per-platform component group dir.
pub const POOL_TARGET: &str = if cfg!(target_os = "windows") {
    "win-x64"
} else if cfg!(target_os = "macos") {
    "mac-arm64"
} else if cfg!(target_arch = "aarch64") {
    "linux-arm64"
} else {
    "linux-x64"
};

/// Resolve a `components/` root to the actual pool dir this target reads.
pub fn resolve_pool(base: &Path) -> Option<PathBuf> {
    let group = base.join(POOL_TARGET);
    if group.join("manifest.json").exists() {
        return Some(group);
    }
    if base.join("manifest.json").exists() {
        return Some(base.to_path_buf());
    }
    None
}

/// The drive root (where models/ + data/ live) for a resolved pool dir.
pub fn pool_drive_root(comp: &Path) -> Option<PathBuf> {
    let mut p = Some(comp);
    while let Some(cur) = p {
        if cur.file_name().is_some_and(|n| n == "components") {
            return cur.parent().map(Path::to_path_buf);
        }
        p = cur.parent();
    }
    comp.parent().map(Path::to_path_buf)
}

pub fn components_dir(here: &Path) -> Option<PathBuf> {
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

/// NixOS detection (always false off linux).
pub fn is_nixos() -> bool {
    if std::env::var_os("PLANAI_NIX_LD").is_some() {
        return true;
    }
    cfg!(target_os = "linux")
        && (Path::new("/etc/NIXOS").exists() || Path::new("/run/current-system/sw").exists())
}

/// NixOS FHS entry: squashfuse-mount (or extract) the FHS-closure store, then in an
/// outer bubblewrap namespace provide it as /nix/store and re-exec OURSELF as a child
/// (PLANAI_FHS_REEXEC=1). Returns the child's exit code, or None when the FHS path
/// doesn't apply / can't be set up (caller runs bare).
#[cfg(target_os = "linux")]
pub fn maybe_run_in_fhs(exe: &Path, comp: Option<&Path>, spinner: &SpinnerHandle) -> Option<i32> {
    if std::env::var_os("PLANAI_FHS_REEXEC").is_some()
        || std::env::var_os("PLANAI_DEV").is_some()
        || !is_nixos()
    {
        return None;
    }
    let comp = comp?;
    let wrapper = fs::read_to_string(comp.join("nixos-fhs.path")).ok()?.trim().to_string();
    if wrapper.is_empty() {
        return None;
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
    kill_spinner(spinner);
    let mode = fhs_store_mode(&bwrap);
    log(&format!("NixOS FHS: entering sandbox (store provided via {mode})"));
    let self_exe = exe.to_path_buf();
    let mut cmd = Command::new(&bwrap);
    cmd.args(["--dev-bind", "/", "/"]);
    if mode == "overlay" {
        cmd.arg("--overlay-src").arg(&store_root)
            .arg("--overlay-src").arg("/nix/store")
            .args(["--ro-overlay", "/nix/store"]);
    } else if let Ok(rd) = fs::read_dir(&store_root) {
        let mut names: Vec<_> = rd.flatten().map(|e| e.file_name()).collect();
        names.sort();
        for name in names {
            cmd.arg("--ro-bind").arg(store_root.join(&name)).arg(Path::new("/nix/store").join(&name));
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

/// Probe unprivileged overlayfs via the embedded bwrap, returning "overlay" or "bind".
#[cfg(target_os = "linux")]
pub fn fhs_store_mode(bwrap: &Path) -> &'static str {
    let probe = cache_root().join("ovl-probe");
    let low = probe.join("low");
    let low2 = probe.join("low2");
    let _ = fs::create_dir_all(&low);
    let _ = fs::create_dir_all(&low2);
    let _ = fs::write(low.join("marker"), b"ok");
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

/// Single-instance guard: exclusive advisory lock on a file in the cache root.
/// Ok(File) = locked (keep alive). Err(true) = another instance holds it.
/// Err(false) = couldn't lock (proceed anyway).
pub fn acquire_instance_lock() -> Result<std::fs::File, bool> {
    use fs2::FileExt;
    let path = cache_root().join("instance.lock");
    if let Some(p) = path.parent() {
        let _ = fs::create_dir_all(p);
    }
    let file = match fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path) {
        Ok(f) => f,
        Err(e) => {
            log(&format!("instance lock: cannot open {} ({e}) — skipping guard", path.display()));
            return Err(false);
        }
    };
    match file.try_lock_exclusive() {
        Ok(()) => Ok(file),
        Err(_) => Err(true),
    }
}

/// The prepared component tree (`PLANAI_RESOURCES`), where the mounted components
/// (runtime/ ollama/ ow-assets/ …) live. Falls back to `dist` for dev layouts.
pub fn resources_root() -> PathBuf {
    std::env::var_os("PLANAI_RESOURCES").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("dist"))
}

/// The writable USB-side root holding models/ + data/ (`PLANAI_PORTABLE_ROOT`); else
/// the launcher's own dir. The update apply + relaunch resolve the drive through this.
pub fn portable_root() -> PathBuf {
    if let Some(p) = std::env::var_os("PLANAI_PORTABLE_ROOT") {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Create `p` (best-effort) and return it.
pub fn ensure_dir(p: PathBuf) -> PathBuf {
    let _ = std::fs::create_dir_all(&p);
    p
}

pub fn cache_root() -> PathBuf {
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

/// Pick the component flavour base name present in `comp` for this OS's format.
pub fn pick_base(comp: &Path, prefix: &str) -> Option<String> {
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

/// Write an embedded tool to `dir` once and return its path.
pub fn ensure_tool(dir: &Path, name: &str, bytes: &[u8]) -> Option<PathBuf> {
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

pub fn is_mountpoint(p: &Path) -> bool {
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

pub fn find_fusermount() -> Option<String> {
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

/// Tear down a stale mount leaked by a crashed prior run before remounting onto it.
#[cfg(unix)]
pub fn detach_stale_mount(dest: &Path) {
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
            let _ = Command::new(&fm).arg("-uz").arg(dest).status();
        }
    }
}

/// Make a component available at `dest`. Returns how it was provided (for teardown).
pub fn provide(comp: &Path, base: &str, dest: &Path, tools_dir: &Path, force_extract: bool) -> std::io::Result<MountKind> {
    #[cfg(target_os = "windows")]
    {
        let dir = comp.join(base);
        if dir.is_dir() {
            let _ = fs::remove_dir_all(dest);
            if let Some(parent) = dest.parent() { let _ = fs::create_dir_all(parent); }
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
        return Err(std::io::Error::other("mount/extract squashfs failed"));
    }
    let dmg = comp.join(format!("{base}.dmg"));
    if dmg.exists() {
        fs::create_dir_all(dest)?;
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
        return Err(std::io::Error::other("hdiutil attach failed"));
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("component {base} not found")))
}

#[allow(dead_code)] // used only on windows (dir-in-place fallback)
pub fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
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

/// Unmount/detach the component mounts in order (lazy-detach on busy).
pub fn teardown(mounts: &[Mount]) {
    for m in mounts {
        match m.kind {
            MountKind::Fuse => {
                let fm = find_fusermount().unwrap_or_else(|| "fusermount".into());
                let ok = Command::new(&fm).arg("-u").arg(&m.dest).status().map(|s| s.success()).unwrap_or(false);
                if !ok {
                    let _ = Command::new(&fm).arg("-uz").arg(&m.dest).status();
                }
            }
            MountKind::Dmg => {
                let ok = Command::new("hdiutil").arg("detach").arg(&m.dest).status().map(|s| s.success()).unwrap_or(false);
                if !ok {
                    let _ = Command::new("hdiutil").arg("detach").arg("-force").arg(&m.dest).status();
                }
            }
            MountKind::None => {}
        }
    }
}

/// Is `bin` an executable on $PATH?
#[cfg(target_os = "linux")]
fn in_path(bin: &str) -> bool {
    which::which(bin).is_ok()
}

/// Options for the splash window.
#[derive(Clone, Copy)]
pub struct SplashOpts<'a> {
    pub text: &'a str,
    pub progress: bool,
}

#[cfg(target_os = "linux")]
fn spawn_system_progress_dialog(opts: SplashOpts) -> Option<std::process::Child> {
    use std::io::IsTerminal;
    use std::process::Stdio;
    let title = "plan.ai";
    let text = if opts.text.is_empty() { "Starting…".to_string() } else { opts.text.to_string() };
    let have_display = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|k| std::env::var_os(k).map(|v| !v.is_empty()).unwrap_or(false));
    if have_display && in_path("zenity") {
        let mut c = Command::new("zenity");
        c.args(["--progress", "--no-cancel", "--auto-close", "--width=360"]);
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
    if have_display && in_path("kdialog") {
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

/// Write the embedded splash spinner (or a PLANAI_SPINNER dev override) + chmod +x.
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

/// Probe ONCE whether the embedded splash window can come up here (cached).
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

/// Spawn the embedded splash window in the chosen mode.
fn spawn_splash_window(opts: SplashOpts) -> Option<std::process::Child> {
    use std::process::Stdio;
    let bin = materialize_spinner()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("--text").arg(opts.text);
    if opts.progress {
        cmd.arg("--progress").stdin(Stdio::piped());
    } else {
        cmd.arg("--watch-stdin").stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::null());
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

/// Show the splash — the one entry point every progress site uses.
pub fn show_splash(opts: SplashOpts) -> Option<Splash> {
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
    Some(fallback_splash(opts))
}

/// The non-GUI splash, tried in order, ALWAYS yielding something.
fn fallback_splash(opts: SplashOpts) -> Splash {
    #[cfg(target_os = "linux")]
    if let Some(child) = spawn_system_progress_dialog(opts) {
        return Splash::Proc(child);
    }
    if let Some(s) = term_splash(opts) {
        return s;
    }
    let text = if opts.text.is_empty() { "Starting…".to_string() } else { opts.text.to_string() };
    notify("plan.ai", &text);
    log("splash: no GUI/dialog/tty available — showed a desktop notification");
    Splash::Notify
}

fn term_splash(opts: SplashOpts) -> Option<Splash> {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        log("splash: terminal progress line (no GUI/dialog available)");
        return Some(Splash::Term(TermBar::new(opts.text.to_string(), opts.progress)));
    }
    None
}
