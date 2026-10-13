use std::env;
use std::path::PathBuf;

fn main() {
    let gpod = pkg_config::Config::new()
        .atleast_version("0.8")
        .probe("libgpod-1.0")
        .expect("libgpod-1.0 not found by pkg-config (pacman -S libgpod)");

    println!("cargo:rerun-if-changed=wrapper.h");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args(gpod.include_paths.iter().map(|p| format!("-I{}", p.display())))
        .allowlist_function("itdb_.*")
        .allowlist_type("Itdb_.*")
        .allowlist_var("ITDB_.*")
        // GLib types come from glib-sys so they interoperate with it.
        .blocklist_type("_?G.*")
        .blocklist_type("g(char|uchar|int|uint|short|ushort|long|ulong|boolean|pointer|constpointer|size|ssize|float|double|int8|uint8|int16|uint16|int32|uint32|int64|uint64|unichar)")
        .raw_line("use glib_sys::*;")
        .derive_default(false)
        .layout_tests(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen failed on gpod/itdb.h");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings
        .write_to_file(out)
        .expect("could not write bindings");
}
