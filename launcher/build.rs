// Embed the static squashfuse_ll + unsquashfs into the LINUX launcher so the rust
// binary mounts the component squashfs itself (like the AppImage runtime links
// squashfuse). The nix build passes their store paths via env; on other targets
// nothing is embedded (mac uses hdiutil, windows uses pre-extracted dirs).
use std::{env, fs, path::Path};

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let out = env::var("OUT_DIR").unwrap();
    // Always create the files so include_bytes! compiles on every target; they
    // are only non-empty (and only used) on linux.
    for (var, name) in [("PLANAI_SQUASHFUSE_LL", "squashfuse_ll"), ("PLANAI_UNSQUASHFS", "unsquashfs")] {
        let dst = Path::new(&out).join(name);
        let bytes = if target_os == "linux" {
            match env::var(var) {
                Ok(p) if !p.is_empty() => fs::read(&p).unwrap_or_else(|e| panic!("read {var}={p}: {e}")),
                _ => panic!("{var} must point at the static {name} for the linux launcher"),
            }
        } else {
            Vec::new()
        };
        fs::write(&dst, bytes).unwrap();
        println!("cargo:rerun-if-env-changed={var}");
    }

    // Embed the splash spinner for THIS target (ALL OSes — unlike squashfuse), fed
    // by the flake (PLANAI_SPINNER_BIN = the cross-built spinnerFor output). Empty
    // when unset (dev / bare cargo): the launcher then falls back to the
    // PLANAI_SPINNER dev path, or a system dialog on linux.
    {
        let dst = Path::new(&out).join("spinner");
        let bytes = match env::var("PLANAI_SPINNER_BIN") {
            Ok(p) if !p.is_empty() => {
                fs::read(&p).unwrap_or_else(|e| panic!("read PLANAI_SPINNER_BIN={p}: {e}"))
            }
            _ => Vec::new(),
        };
        fs::write(&dst, bytes).unwrap();
        println!("cargo:rerun-if-env-changed=PLANAI_SPINNER_BIN");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
