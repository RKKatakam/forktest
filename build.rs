//! On macOS, compiles `shim/tzfix.c` into a universal dylib the binary embeds. See `python::tzfix_path`.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=shim/tzfix.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("tzfix.dylib");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args(["-dynamiclib", "-O2", "-arch", "arm64", "-arch", "x86_64", "-mmacosx-version-min=11.0", "-o"])
        .arg(&out)
        .arg("shim/tzfix.c")
        .status()
        .unwrap_or_else(|e| panic!("running {cc} to build shim/tzfix.c: {e}"));
    assert!(status.success(), "{cc} failed to build shim/tzfix.c");
}
