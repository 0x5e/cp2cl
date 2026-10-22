//! Puts `assets/` into the binary, and what git says about the commit.
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
//!
//! The version and the commit are read here as well, for the same reason: a
//! version is a name given to a commit, so the commit being built is what knows
//! what the build is called, and a file holding it would only be right until
//! the next commit.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let root = manifest.join("assets");
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

    // Watched as a whole rather than file by file: a checkout moves `HEAD`, a
    // commit moves a branch, and a tag is a third file again — any of them can
    // be what changes the answer, and a directory is one line that covers all.
    println!("cargo:rerun-if-changed=.git");
    let version = git(&manifest, &["describe", "--tags", "--exact-match"])
        .unwrap_or_else(|| "dev".to_string());
    // A tree that does not match the commit is not that commit's build: what
    // ends up in the binary — `assets/` above all — is whatever was in the
    // working tree at the time. Said in the word `git describe --dirty` uses,
    // so a build from a modified tree is recognisable at a glance.
    let mut commit = git(&manifest, &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string());
    if dirty(&manifest) {
        commit.push_str("-dirty");
    }
    println!("cargo:rustc-env=CP2CL_VERSION={version}");
    println!("cargo:rustc-env=CP2CL_COMMIT={commit}");
}

/// What git says about the commit being built, or nothing when it cannot say.
///
/// A commit with no tag on it is `dev`: it has not been given a version, and
/// inventing one would say it had. A build with no checkout at all is the same
/// answer, since there is nothing to name either way.
fn git(manifest: &Path, args: &[&str]) -> Option<String> {
    let said = std::process::Command::new("git")
        .current_dir(manifest)
        .args(args)
        .output()
        .ok()?;
    if !said.status.success() {
        return None;
    }
    let text = String::from_utf8(said.stdout).ok()?;
    text.lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Whether the working tree holds anything the commit does not.
///
/// `status` rather than `diff-index`: an untracked file under `assets/` goes
/// into the binary exactly as a changed one does. Ignored paths are left out of
/// it, which is what keeps `target/` and the built-in credentials from making
/// every build dirty. A tree git cannot be asked about is not called dirty —
/// the hash is still the right answer there.
fn dirty(manifest: &Path) -> bool {
    match std::process::Command::new("git")
        .current_dir(manifest)
        .args(["status", "--porcelain"])
        .output()
    {
        Ok(said) => said.status.success() && !said.stdout.is_empty(),
        Err(_) => false,
    }
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
