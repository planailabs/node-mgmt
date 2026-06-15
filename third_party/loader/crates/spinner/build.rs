// Bake the splash colour scheme at build time from env (set by the consumer's flake
// from loader.toml [spinner]), so the spinner crate carries no hardcoded brand. Each
// var is an `rrggbb` hex string; unset → the default below. Writes $OUT_DIR/colors.rs
// (a few `const`s) which main.rs include!s.
use std::{env, fs, path::Path};

fn rgb(var: &str, default: (u8, u8, u8)) -> (u8, u8, u8) {
    println!("cargo:rerun-if-env-changed={var}");
    match env::var(var).ok().filter(|s| !s.is_empty()) {
        Some(s) => {
            let h = s.trim_start_matches('#');
            let p = |i: usize| u8::from_str_radix(h.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0);
            if h.len() == 6 {
                (p(0), p(2), p(4))
            } else {
                eprintln!("spinner: {var}='{s}' is not rrggbb — using default");
                default
            }
        }
        None => default,
    }
}

fn main() {
    // Defaults = the historical plan.ai palette (so an unconfigured build is unchanged).
    let canvas = rgb("PLANAI_SPINNER_CANVAS", (0x1c, 0x27, 0x35));
    let brand = rgb("PLANAI_SPINNER_BRAND", (0xf9, 0x73, 0x16));
    let text = rgb("PLANAI_SPINNER_TEXT", (0xe8, 0xed, 0xf5));
    let track = rgb("PLANAI_SPINNER_TRACK", (0x2d, 0x3c, 0x50));
    let canvas_u32 = ((canvas.0 as u32) << 16) | ((canvas.1 as u32) << 8) | (canvas.2 as u32);
    let body = format!(
        "const CANVAS: u32 = {canvas_u32:#08x};\n\
         const BRAND: (u8, u8, u8) = ({}, {}, {});\n\
         const TEXT: (u8, u8, u8) = ({}, {}, {});\n\
         const TRACK: (u8, u8, u8) = ({}, {}, {});\n",
        brand.0, brand.1, brand.2, text.0, text.1, text.2, track.0, track.1, track.2,
    );
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("colors.rs"), body).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
