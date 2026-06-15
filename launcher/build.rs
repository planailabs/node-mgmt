// The embedded mount/FHS/splash binaries (squashfuse_ll/unsquashfs/bwrap/spinner)
// moved into loader-core's build.rs (they're consumed by the loader-core runtime
// substrate). This build.rs now only wires the SPA assets that serve.rs rust-embeds.
use std::{env, fs};

fn main() {
    // Where serve.rs's rust-embed finds the built Dioxus SPA assets. The nix build
    // passes the `spa` derivation via PLANAI_SPA_DIST; dev/bare cargo falls back to
    // the in-tree `spa/` dir (created empty if missing so the build compiles).
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
    println!("cargo:rerun-if-changed=build.rs");
}
