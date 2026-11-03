//! The user's toolchain configuration.
//!
//! A compiler cannot infer where this project's C library lives, and it must
//! not guess: `~/.shycc.toml` names it, so an installed toolchain works
//! wherever the library was put.
//!
//! ```toml
//! shylib = "/home/me/ShyProject/libshy"
//! ```
//!
//! A checkout needs no file: the library sits next to the crates, which is what
//! `shylib` falls back to when the file is missing or silent.

use std::fs;
use std::path::{Path, PathBuf};

/// The `libshy` directory, from the configuration file or the checkout.
pub fn shylib() -> Option<PathBuf> {
    if let Some(configured) = configured() {
        return Some(configured);
    }
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .join("libshy");
    checkout.is_dir().then_some(checkout)
}

/// The include directories `libshy` contributes, in lookup order: the C
/// headers, the source layer, and the compatibility wrappers.
pub fn shylib_includes(shylib: &Path) -> [PathBuf; 3] {
    [
        shylib.join("c"),
        shylib.to_path_buf(),
        shylib.join("include"),
    ]
}

/// The `shylib` setting of `~/.shycc.toml`.
///
/// The file is read with a line-oriented reader instead of a TOML parser
/// because one string key is all this configuration has; anything else in the
/// file is ignored.
fn configured() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let text = fs::read_to_string(PathBuf::from(home).join(".shycc.toml")).ok()?;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "shylib" {
            continue;
        }
        let value = value.trim().trim_matches(['"', '\'']);
        if !value.is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    None
}
