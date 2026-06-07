// plan.ai native launcher — a tiny, dependency-free Rust binary that makes the
// mac/windows dists self-launchable from the USB and is the first piece of logic
// moved to Rust.
//
// It sits beside the shared component pool and the Electron app on the USB:
//   <root>/plan-ai(.exe)            this launcher
//   <root>/components/  tools/      shared pool (loader reads these)
//   <root>/plan.ai.app | plan.ai.exe   the bundled Electron app
//
// It points the loader at the pool (PLANAI_COMPONENTS / PLANAI_MOUNT_TOOLS via
// paths relative to itself — robust regardless of where the USB is mounted) and
// then launches the Electron app, inheriting that environment.
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// First of `cands` (relative to `base` and one level up) that exists.
fn find_near(base: &Path, name: &str) -> Option<PathBuf> {
    for root in [Some(base), base.parent()].into_iter().flatten() {
        let p = root.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Locate the bundled Electron executable beside the launcher.
fn electron_target(here: &Path) -> Option<(PathBuf, Vec<String>)> {
    // macOS: the .app bundle's inner binary
    for root in [Some(here), here.parent()].into_iter().flatten() {
        let mac = root.join("plan.ai.app/Contents/MacOS/plan.ai");
        if mac.exists() {
            return Some((mac, vec![]));
        }
    }
    // windows / generic: plan.ai.exe (or plan.ai) beside the launcher or one up
    for root in [Some(here), here.parent()].into_iter().flatten() {
        for name in ["plan.ai.exe", "plan.ai"] {
            let p = root.join(name);
            // don't recurse into ourselves
            if p.exists() && env::current_exe().map(|e| e != p).unwrap_or(true) {
                return Some((p, vec![]));
            }
        }
    }
    None
}

fn main() {
    let exe = env::current_exe().expect("current_exe");
    let here = exe.parent().expect("exe parent").to_path_buf();

    // Point the in-app loader at the shared pool beside us (unless already set).
    if env::var_os("PLANAI_COMPONENTS").is_none() {
        if let Some(c) = find_near(&here, "components") {
            env::set_var("PLANAI_COMPONENTS", &c);
        }
    }
    if env::var_os("PLANAI_MOUNT_TOOLS").is_none() {
        if let Some(t) = find_near(&here, "tools") {
            env::set_var("PLANAI_MOUNT_TOOLS", t.join("bin"));
        }
    }

    let (program, extra) = match electron_target(&here) {
        Some(t) => t,
        None => {
            eprintln!("plan.ai: could not find the bundled app (plan.ai.app / plan.ai.exe) beside {}", here.display());
            std::process::exit(1);
        }
    };

    let mut cmd = Command::new(&program);
    cmd.args(&extra);
    cmd.args(env::args_os().skip(1)); // forward user args

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec(); // replace this process with Electron
        eprintln!("plan.ai: failed to exec {}: {err}", program.display());
        std::process::exit(1);
    }
    #[cfg(windows)]
    {
        match cmd.status() {
            Ok(st) => std::process::exit(st.code().unwrap_or(0)),
            Err(e) => {
                eprintln!("plan.ai: failed to start {}: {e}", program.display());
                std::process::exit(1);
            }
        }
    }
}
