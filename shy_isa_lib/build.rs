use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=include/shy_types.h");
    println!("cargo:rerun-if-changed=include/shy_file.h");
    println!("cargo:rerun-if-changed=src/file.c");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let include_dir = manifest_dir.join("include");
    let file_header = include_dir.join("shy_file.h");

    cc::Build::new()
        .file(manifest_dir.join("src/file.c"))
        .include(&include_dir)
        .flag_if_supported("-std=c11")
        .compile("shy_file");

    let file_bindings = bindgen::Builder::default()
        .header(file_header.to_string_lossy())
        .clang_arg(format!("-I{}", include_dir.display()))
        .allowlist_type("ShyFile")
        .allowlist_function("shy_.*")
        .allowlist_var("SHY_FILE_NAME_MAX")
        .derive_default(true)
        .generate()
        .expect("failed to generate shy_file bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    file_bindings
        .write_to_file(out_path.join("shy_file_bindings.rs"))
        .expect("failed to write shy_file bindings");
}
