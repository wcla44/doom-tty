//! Compiles the original Doom engine (doomgeneric, GPL-2.0) as a static library.

use std::path::Path;

// Platform backends and sound/music drivers we replace with the Rust frontend.
const SKIP: &[&str] = &["doomgeneric_", "i_allegro", "i_sdl"];

fn main() {
    let dir = Path::new("vendor-doomgeneric/doomgeneric");
    let mut build = cc::Build::new();
    for entry in std::fs::read_dir(dir).expect("vendor-doomgeneric is missing") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.ends_with(".c") && !SKIP.iter().any(|s| name.starts_with(s)) {
            build.file(&path);
        }
    }
    build
        .include(dir)
        .define("NORMALUNIX", None)
        .define("_DEFAULT_SOURCE", None)
        .opt_level(2)
        .warnings(false)
        .flag_if_supported("-w")
        .compile("doomgeneric");
    println!("cargo:rerun-if-changed={}", dir.display());
}
