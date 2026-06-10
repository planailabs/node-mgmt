//! Standalone smoke test for the splash spinner: launch the BUILT binary with
//! `--selftest` and assert it opens a window, paints a few frames, and exits 0.
//! This is the "does the spinner actually show?" check the launcher can't make for
//! itself (it spawns the spinner with stdout/stderr to /dev/null).
//!
//! The eframe binary dlopens the system X11/GL libs at runtime. On a normal distro
//! they're on the loader path and it runs directly. On NixOS they aren't — so we run
//! the spinner through the project's FHS wrapper (`.#nixosFhs` → `planai-fhs`, the
//! same bubblewrap env the launcher uses for Electron on NixOS), which provides them.
//! That way the spinner is ALWAYS exercised for real, never skipped on NixOS.
//!
//! Only skipped when there's no display at all (headless CI), since the window needs
//! one. Run locally: `cargo test -p plan-ai-spinner`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn have_display() -> bool {
    let set = |k: &str| std::env::var_os(k).map(|v| !v.is_empty()).unwrap_or(false);
    set("DISPLAY") || set("WAYLAND_DISPLAY")
}

fn is_nixos() -> bool {
    Path::new("/etc/NIXOS").exists() || Path::new("/run/current-system/sw").exists()
}

/// Build (cached) the repo's FHS wrapper and return its `planai-fhs` runner, so the
/// dynamic eframe binary can load X11/GL on NixOS. None if `nix` isn't available.
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

#[test]
fn spinner_selftest_renders() {
    if !have_display() {
        eprintln!("no DISPLAY/WAYLAND_DISPLAY — skipping spinner selftest (headless)");
        return;
    }
    // Cargo sets this to the built binary's path for integration tests.
    let bin = env!("CARGO_BIN_EXE_plan-ai-spinner");

    // On NixOS, always go through the FHS wrapper (the launcher's own GUI path there).
    let mut cmd = if is_nixos() {
        match fhs_runner() {
            Some(fhs) => {
                eprintln!("NixOS — running spinner via FHS wrapper {}", fhs.display());
                let mut c = Command::new(fhs);
                c.arg(bin);
                c
            }
            None => {
                eprintln!(
                    "NixOS but couldn't build .#nixosFhs (is `nix` available?) — skipping; \
                     the FHS wrapper is required to load X11/GL for the eframe binary here"
                );
                return;
            }
        }
    } else {
        Command::new(bin)
    };

    let mut child = cmd
        .args(["--selftest", "--text", "selftest"])
        .spawn()
        .expect("spawn plan-ai-spinner --selftest");

    // The selftest closes after a few frames / 5s; give it a generous bound.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            None => {
                let _ = child.kill();
                panic!("spinner --selftest did not exit within 30s (window may be hung)");
            }
        }
    };

    assert!(
        status.success(),
        "spinner --selftest exited with {:?} — the splash did not come up in this environment",
        status.code()
    );
}
