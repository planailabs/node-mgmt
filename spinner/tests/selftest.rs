//! Standalone smoke + lifecycle tests for the splash spinner: launch the BUILT
//! binary and assert it opens a window, paints, and — crucially — closes on
//! exactly the right signals and no others:
//!   - `--selftest` renders a few frames and exits 0 (the "does it show?" probe).
//!   - `--watch-stdin` stays up while the launcher-side pipe is open and closes
//!     promptly on EOF (parent death). There is deliberately NO wall-clock
//!     lifetime — the old 180s cap killed the splash mid-update.
//!   - no flag + an inherited /dev/null stdin must NOT close it (the launcher
//!     kills it instead).
//!   - `--progress` closes on stdin EOF and on a `100` protocol line.
//!
//! The winit binary dlopens the system X11/GL libs at runtime. On a normal distro
//! they're on the loader path and it runs directly. On NixOS they aren't — so we run
//! the spinner through the project's FHS wrapper (`.#nixosFhs` → `planai-fhs`, the
//! same bubblewrap env the launcher uses for Electron on NixOS), which provides them.
//! That way the spinner is ALWAYS exercised for real, never skipped on NixOS.
//!
//! Only skipped when there's no display at all (headless CI), since the window needs
//! one. Run locally: `cargo test -p plan-ai-spinner`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn have_display() -> bool {
    let set = |k: &str| std::env::var_os(k).map(|v| !v.is_empty()).unwrap_or(false);
    set("DISPLAY") || set("WAYLAND_DISPLAY")
}

fn is_nixos() -> bool {
    Path::new("/etc/NIXOS").exists() || Path::new("/run/current-system/sw").exists()
}

/// Build (cached) the repo's FHS wrapper and return its `planai-fhs` runner, so the
/// dynamic winit binary can load X11/GL on NixOS. None if `nix` isn't available.
fn fhs_runner() -> Option<PathBuf> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
    let out = Command::new("nix")
        .args(["build", "--no-link", "--print-out-paths", ".#nixosFhs"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "nix build .#nixosFhs failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).lines().last()?.trim().to_string();
    let runner = PathBuf::from(path).join("bin").join("planai-fhs");
    runner.exists().then_some(runner)
}

/// A `Command` that runs the built spinner in THIS environment (FHS-wrapped on
/// NixOS), or None when the test should skip: headless, or NixOS without `nix`.
/// The FHS-or-direct decision is made once and shared across tests.
fn spinner_command() -> Option<Command> {
    // None = direct, Some(path) = via the FHS runner.
    static RUNNER: OnceLock<Option<Option<PathBuf>>> = OnceLock::new();
    if !have_display() {
        eprintln!("no DISPLAY/WAYLAND_DISPLAY — skipping spinner test (headless)");
        return None;
    }
    let runner = RUNNER.get_or_init(|| {
        if !is_nixos() {
            return Some(None);
        }
        match fhs_runner() {
            Some(fhs) => {
                eprintln!("NixOS — running spinner via FHS wrapper {}", fhs.display());
                Some(Some(fhs))
            }
            None => {
                eprintln!(
                    "NixOS but couldn't build .#nixosFhs (is `nix` available?) — skipping; \
                     the FHS wrapper is required to load X11/GL for the winit binary here"
                );
                None
            }
        }
    });
    // Cargo sets this to the built binary's path for integration tests.
    let bin = env!("CARGO_BIN_EXE_plan-ai-spinner");
    match runner {
        Some(Some(fhs)) => {
            let mut c = Command::new(fhs);
            c.arg(bin);
            Some(c)
        }
        Some(None) => Some(Command::new(bin)),
        None => None,
    }
}

/// Poll-wait for the child to exit, up to `max`. None = still running at the
/// deadline (caller decides whether that's a pass or a failure).
fn wait_up_to(child: &mut Child, max: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + max;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => return Some(s),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            None => return None,
        }
    }
}

/// Assert the child is still running `after` from now (the window must not
/// self-close), without consuming its exit status on failure paths.
fn assert_stays_up(child: &mut Child, after: Duration, why: &str) {
    if let Some(status) = wait_up_to(child, after) {
        panic!("spinner exited early ({status}) — {why}");
    }
}

fn kill_and_reap(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn spinner_selftest_renders() {
    let Some(mut cmd) = spinner_command() else { return };
    let mut child = cmd
        .args(["--selftest", "--text", "selftest"])
        .spawn()
        .expect("spawn plan-ai-spinner --selftest");

    // The selftest closes after a few frames / 5s; give it a generous bound.
    let status = match wait_up_to(&mut child, Duration::from_secs(30)) {
        Some(s) => s,
        None => {
            kill_and_reap(child);
            panic!("spinner --selftest did not exit within 30s (window may be hung)");
        }
    };

    assert!(
        status.success(),
        "spinner --selftest exited with {:?} — the splash did not come up in this environment",
        status.code()
    );
}

/// `--watch-stdin`: the launcher pipes stdin and holds it open — the splash must
/// stay up for as long as the pipe lives (no wall-clock self-close; the old 180s
/// max-lifetime killed it mid-update) and close promptly once the pipe drops
/// (= the launcher died).
#[test]
fn watch_stdin_stays_until_eof_then_closes() {
    let Some(mut cmd) = spinner_command() else { return };
    let mut child = cmd
        .args(["--watch-stdin", "--text", "watch test"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn plan-ai-spinner --watch-stdin");
    let stdin = child.stdin.take().expect("piped stdin");

    assert_stays_up(&mut child, Duration::from_secs(3), "must stay up while the launcher-side pipe is open");

    drop(stdin); // simulate launcher death → EOF
    let status = match wait_up_to(&mut child, Duration::from_secs(10)) {
        Some(s) => s,
        None => {
            kill_and_reap(child);
            panic!("spinner did not close within 10s of stdin EOF (--watch-stdin)");
        }
    };
    assert!(status.success(), "spinner exited with {status} after stdin EOF");
}

/// Without `--watch-stdin` or `--progress` the spinner must NOT read stdin: an
/// inherited /dev/null (instant EOF) must not close it. The launcher kills it.
#[test]
fn indeterminate_ignores_inherited_null_stdin() {
    let Some(mut cmd) = spinner_command() else { return };
    let mut child = cmd
        .args(["--text", "no watch"])
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn plan-ai-spinner (indeterminate)");

    assert_stays_up(
        &mut child,
        Duration::from_secs(3),
        "an inherited /dev/null EOF must not close the indeterminate splash",
    );
    kill_and_reap(child);
}

/// `--progress`: stdin EOF (the launcher closed the pipe or died) closes the bar.
#[test]
fn progress_closes_on_stdin_eof() {
    let Some(mut cmd) = spinner_command() else { return };
    let mut child = cmd
        .args(["--progress", "--text", "progress test"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn plan-ai-spinner --progress");
    let mut stdin = child.stdin.take().expect("piped stdin");

    writeln!(stdin, "30").and_then(|_| stdin.flush()).expect("feed percent");
    assert_stays_up(&mut child, Duration::from_secs(2), "must stay up mid-progress while the pipe is open");

    drop(stdin);
    let status = match wait_up_to(&mut child, Duration::from_secs(10)) {
        Some(s) => s,
        None => {
            kill_and_reap(child);
            panic!("spinner did not close within 10s of stdin EOF (--progress)");
        }
    };
    assert!(status.success(), "spinner exited with {status} after stdin EOF");
}

/// `--progress`: a `100` protocol line closes the bar even with the pipe held open.
#[test]
fn progress_closes_on_100_percent() {
    let Some(mut cmd) = spinner_command() else { return };
    let mut child = cmd
        .args(["--progress", "--text", "progress 100 test"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn plan-ai-spinner --progress");
    let mut stdin = child.stdin.take().expect("piped stdin");

    writeln!(stdin, "100").and_then(|_| stdin.flush()).expect("feed 100");
    let status = match wait_up_to(&mut child, Duration::from_secs(10)) {
        Some(s) => s,
        None => {
            kill_and_reap(child);
            panic!("spinner did not close within 10s of the `100` protocol line");
        }
    };
    assert!(status.success(), "spinner exited with {status} after `100`");
    drop(stdin);
}
