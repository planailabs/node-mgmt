// Embed the static squashfuse_ll + unsquashfs (linux mount), the static bubblewrap
// (NixOS FHS namespace), and the splash spinner (all OSes) into loader-core, fed by
// the consumer's build via env (the flake passes the cross-built store paths). Empty
// when unset (dev / bare cargo / non-applicable target); the files always exist so
// include_bytes! compiles everywhere.
use std::{env, fs, path::Path};

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let out = env::var("OUT_DIR").unwrap();
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
    {
        let dst = Path::new(&out).join("spinner");
        let bytes = match env::var("PLANAI_SPINNER_BIN") {
            Ok(p) if !p.is_empty() => fs::read(&p).unwrap_or_else(|e| panic!("read PLANAI_SPINNER_BIN={p}: {e}")),
            _ => Vec::new(),
        };
        fs::write(&dst, bytes).unwrap();
        println!("cargo:rerun-if-env-changed=PLANAI_SPINNER_BIN");
    }
    {
        let dst = Path::new(&out).join("bwrap");
        let bytes = match env::var("PLANAI_BWRAP_BIN") {
            Ok(p) if !p.is_empty() && target_os == "linux" => fs::read(&p).unwrap_or_else(|e| panic!("read PLANAI_BWRAP_BIN={p}: {e}")),
            _ => Vec::new(),
        };
        fs::write(&dst, bytes).unwrap();
        println!("cargo:rerun-if-env-changed=PLANAI_BWRAP_BIN");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
