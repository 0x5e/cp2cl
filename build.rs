// Builds the macOS Bluetooth bridge, the one thing in this program that is not
// Rust: IOBluetooth has no bindings worth depending on and needs a run loop, so
// it lives in its own small Objective-C process that hands RFCOMM bytes to us
// over a unix socket.
//
// On every other platform this does nothing at all.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=native/cp2cl-bluetooth/main.m");
    println!("cargo:rerun-if-changed=native/cp2cl-bluetooth/Info.plist");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("cp2cl-bluetooth");
    let built = Command::new("clang")
        .args([
            "-fobjc-arc",
            "-framework",
            "Foundation",
            "-framework",
            "AppKit",
            "-framework",
            "IOBluetooth",
        ])
        .args([
            "-sectcreate",
            "__TEXT",
            "__info_plist",
            "native/cp2cl-bluetooth/Info.plist",
        ])
        .args(["native/cp2cl-bluetooth/main.m", "-o"])
        .arg(&out)
        .status();

    match built {
        Ok(status) if status.success() => {
            println!("cargo:rustc-env=CP2CL_BT_HELPER={}", out.display());
        }
        // Not fatal: the wired half still works, and the wireless half says so
        // when it is asked for.
        other => {
            println!(
                "cargo:warning=could not build the Bluetooth bridge ({other:?}); \
                      wireless CarPlay will be unavailable"
            );
            println!("cargo:rustc-env=CP2CL_BT_HELPER=");
        }
    }
}
