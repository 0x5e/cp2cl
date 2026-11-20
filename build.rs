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
//!
//! The CarLife key is chosen here too — the environment first, the extracted
//! file in the checkout second — and written out for the program to include. A
//! build handed neither is stopped here rather than on the board.

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

    let out = Path::new(&std::env::var("OUT_DIR").expect("cargo sets this")).to_path_buf();
    std::fs::write(out.join("assets.rs"), table).expect("the generated table is writable");

    // Two places, the environment first: a build that was handed the key — how
    // the image is built on a machine that never had the extracted file — is a
    // build whose checkout need not hold it. The file is watched only when it is
    // there, like the git paths below: a path that does not exist makes this
    // script run on every build, and that build is exactly the one without it.
    println!("cargo:rerun-if-env-changed=CARLIFE_PRIVATE_KEY");
    if manifest.join(KEY_FILE).exists() {
        println!("cargo:rerun-if-changed={KEY_FILE}");
    }
    let key = carlife_key(&manifest);
    std::fs::write(out.join("carlife.pem"), key).expect("the generated key file is writable");

    // Watched file by file, not as `.git` itself. The `git status` below creates
    // and deletes `.git/index.lock` and rewrites `.git/index`, and a create or a
    // delete in a directory brings that directory's own mtime forward — so
    // watching `.git` made every build find it changed, rerun this script, and
    // rebuild the crate: forty seconds for a binary nothing had touched. These
    // are the paths whose content is the answer — a checkout writes `HEAD`, a
    // commit writes a branch under `refs/heads/`, a tag is a file under
    // `refs/tags/`, and a `git gc` moves both kinds of ref into `packed-refs`.
    // `refs/remotes/` and `refs/stash` are left out: neither can change what
    // this is called. A path that does not exist is skipped rather than named,
    // because a repository that has never been packed has no `packed-refs`.
    for path in [
        ".git/HEAD",
        ".git/refs/heads",
        ".git/refs/tags",
        ".git/packed-refs",
    ] {
        if manifest.join(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
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

/// Where a checkout keeps the extracted CarLife key, from its root.
const KEY_FILE: &str = "authentication/carlife_md_rsa_private.pem";

/// The CarLife key the program answers the car's challenge with.
///
/// Not a credential of ours: it is the CarLife app's own private key, extracted
/// from `com.baidu.carlife`, and the car accepts an answer made with it because
/// Baidu made the matching public half. `CARLIFE_PRIVATE_KEY` is asked first —
/// how a build whose checkout holds no extracted file is handed one, as a PEM
/// block or as the bare Base64 of the app's own constant — and `KEY_FILE`
/// second, which is where an extraction leaves it in the checkout. An empty
/// variable counts as none: `-e CARLIFE_PRIVATE_KEY` in a makefile that was not
/// given one passes the empty string rather than dropping the variable.
///
/// A build with neither is refused here. Running without one is not a way to
/// run any more: the car goes unanswered, asks again every half minute, and the
/// sessions are unauthenticated throughout — a fault to find at compile time
/// rather than on the board.
fn carlife_key(manifest: &Path) -> String {
    let given = std::env::var("CARLIFE_PRIVATE_KEY")
        .ok()
        .map(|pem| pem.trim().to_string())
        .filter(|pem| !pem.is_empty());
    if let Some(pem) = given {
        return pem;
    }
    if let Ok(pem) = std::fs::read_to_string(manifest.join(KEY_FILE)) {
        if !pem.trim().is_empty() {
            return pem;
        }
    }
    panic!("no CarLife key: set CARLIFE_PRIVATE_KEY, or put the extracted key at {KEY_FILE}");
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
        .args(["status", "--porcelain", "--ignore-submodules=dirty"])
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
