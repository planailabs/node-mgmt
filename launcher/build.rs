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

    // Embed the static bubblewrap for the linux launcher's NixOS FHS path (sets up the
    // outer namespace that provides the FHS-closure store over /nix/store). Empty when
    // unset (dev / non-linux): the FHS path is NixOS-prod-only, so a dev build without
    // it just can't enter the sandbox (and doesn't need to).
    {
        let dst = Path::new(&out).join("bwrap");
        let bytes = match env::var("PLANAI_BWRAP_BIN") {
            Ok(p) if !p.is_empty() && target_os == "linux" => {
                fs::read(&p).unwrap_or_else(|e| panic!("read PLANAI_BWRAP_BIN={p}: {e}"))
            }
            _ => Vec::new(),
        };
        fs::write(&dst, bytes).unwrap();
        println!("cargo:rerun-if-env-changed=PLANAI_BWRAP_BIN");
    }

    // Where serve.rs's rust-embed finds the built Dioxus SPA assets. The nix
    // build passes the `spa` derivation via PLANAI_SPA_DIST (so the launcher
    // src no longer carries the assets, and SPA changes rebuild only the
    // embed); dev/bare cargo falls back to the in-tree `spa/` dir produced by
    // scripts/build-spa.sh (created empty if missing so the build compiles).
    {
        let dir = match env::var("PLANAI_SPA_DIST") {
            Ok(p) if !p.is_empty() => p,
            _ => {
                let p = format!("{}/spa", env::var("CARGO_MANIFEST_DIR").unwrap());
                fs::create_dir_all(&p).unwrap();
                p
            }
        };
        println!("cargo:rustc-env=PLANAI_SPA_DIST={dir}");
        println!("cargo:rerun-if-env-changed=PLANAI_SPA_DIST");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
