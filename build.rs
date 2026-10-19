//! Puts `assets/` into the binary.
//!
//! Every file under `assets/` is `include_bytes!`-ed into one table, so what the
//! program carries is read-only data of the executable itself: nothing is
//! unpacked to a temporary directory at startup, and nothing has to be installed
//! next to the executable on the board.
//!
//! The table is generated rather than typed out because a hand-written one only
//! knows the files that existed when it was written. Here, dropping a picture
//! into `assets/` is enough for the program to be able to ask for it by name.
//!
//! Names in the table are relative to `assets/` and use `/`, which is how the
//! rest of the program spells a path.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let root =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this")).join("assets");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", root.display());

    let mut files = Vec::new();
    collect(&root, &root, &mut files);
    files.sort();

    let mut table = String::from("// Written by build.rs from assets/. Do not edit.\n\n");
    table.push_str("static FILES: &[(&str, &[u8])] = &[\n");
    for (name, path) in &files {
        writeln!(table, "    ({name:?}, include_bytes!({path:?})),").expect("a String is writable");
    }
    table.push_str("];\n");

    let out = Path::new(&std::env::var("OUT_DIR").expect("cargo sets this")).join("assets.rs");
    std::fs::write(out, table).expect("the generated table is writable");
}

/// Every file under `dir`, as `(name in the table, path to include)`.
fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).expect("assets/ is readable") {
        let path = entry.expect("the entry is readable").path();
        if path.is_dir() {
            collect(root, &path, files);
            continue;
        }
        let name = path
            .strip_prefix(root)
            .expect("everything walked is under assets/")
            .to_str()
            .expect("asset names are UTF-8")
            .replace('\\', "/");
        let path = path.to_str().expect("asset paths are UTF-8").to_string();
        files.push((name, path));
    }
}
