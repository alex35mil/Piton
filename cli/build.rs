//! Embeds the git commit the binary was built from (`PITON_BUILD`), so a running daemon can say which build it is:
//! `<short sha>`, plus `-dirty` when the working tree had uncommitted changes, or `unknown` outside a git checkout.
//!
//! On macOS, also compiles pi's lift addon (`native/lift.c`, a Node-API module) into `$OUT_DIR/lift.node`, which the
//! binary embeds and the daemon writes out for the pi package (`agent/lift.rs`).

use std::process::Command;

fn main() {
    lift_addon();
    build_id();
}

/// `native/lift.c` as a loadable bundle. Node supplies the `napi_*` symbols when it loads it, so no Node headers or
/// libraries are needed: only clang, which every macOS Rust toolchain already uses to link.
fn lift_addon() {
    println!("cargo:rerun-if-changed=native/lift.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("lift.node");
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        Ok(other) => panic!("no lift addon for architecture {other}"),
        Err(e) => panic!("CARGO_CFG_TARGET_ARCH: {e}"),
    };
    let status = Command::new(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .args([
            "-arch",
            arch,
            "-bundle",
            "-undefined",
            "dynamic_lookup",
            "-O2",
            "-Wall",
            "-Werror",
            "-o",
        ])
        .arg(&out)
        .arg("native/lift.c")
        .status()
        .expect("clang builds the lift addon (native/lift.c)");
    assert!(status.success(), "clang failed to build native/lift.c");
}

fn build_id() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let build = match git(&["rev-parse", "--short=10", "HEAD"]) {
        Some(sha) => {
            // Untracked files don't change the binary; tracked changes do.
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            if dirty { format!("{sha}-dirty") } else { sha }
        }
        None => "unknown".to_string(),
    };
    println!("cargo:rustc-env=PITON_BUILD={build}");
    // Rebuild when HEAD moves or the index changes (a commit, a checkout, staging).
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
        if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{head}");
        }
    }
}
