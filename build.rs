//! Embeds the app icon into the Windows executable.
//!
//! The `.rc` file references `assets/icon.ico`, which is generated from
//! `src/icon_art.rs` by `tools/gen_icon.rs` and committed to the repo — so a
//! normal build never regenerates it, it only embeds it.
//!
//! Cross-toolchain reality: a resource compiler must be available for the
//! target being built (`rc.exe`/`llvm-rc` for MSVC, `windres` for GNU). When
//! one is missing — e.g. checking the Windows GNU target from Linux without a
//! mingw toolchain installed — we warn and skip instead of failing the build.
//! The executable then ships without an embedded icon, while the installer
//! still puts the icon on shortcuts and the uninstaller.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    // Only Windows targets get a resource section.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if !resource_compiler_available() {
        println!(
            "cargo:warning=no Windows resource compiler found \
             (install mingw-w64 for the GNU target, or build with MSVC) — \
             the .exe will have no embedded icon; the installer still sets one."
        );
        return;
    }

    let rc = PathBuf::from("assets/app.rc");
    match embed_resource::compile(rc, embed_resource::NONE) {
        embed_resource::CompilationResult::Ok => {
            println!("cargo:warning=icon embedded from assets/icon.ico")
        }
        embed_resource::CompilationResult::NotAttempted(reason) => {
            println!("cargo:warning=icon not embedded ({reason})");
        }
        embed_resource::CompilationResult::Failed(reason) => {
            println!("cargo:warning=could not embed the icon: {reason}");
        }
        embed_resource::CompilationResult::NotWindows => {}
    }
}

/// Is a resource compiler for the current target on PATH?
fn resource_compiler_available() -> bool {
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let candidates: &[&str] = if target_env == "gnu" {
        &["x86_64-w64-mingw32-windres", "windres"]
    } else {
        &["llvm-rc", "rc.exe", "windres"]
    };
    let path = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&path).any(|dir| {
        candidates.iter().any(|exe| {
            // Windows host binaries carry an extension; accept both forms.
            let mut names = vec![dir.join(exe)];
            if target_env != "gnu" {
                names.push(dir.join(format!("{exe}.exe")));
            }
            names.iter().any(|candidate| candidate.is_file())
        })
    })
}