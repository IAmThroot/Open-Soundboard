use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=Cargo.toml");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Keep the Explorer version metadata in lockstep with Cargo.toml instead of
    // maintaining a second hard-coded copy in assets/app.rc.
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let core = version.split(['-', '+']).next().unwrap_or(&version);
    let mut parts = core.split('.').map(|part| part.parse::<u16>().unwrap_or(0));
    let file_version = format!(
        "{},{},{},{}",
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0)
    );

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let icon = manifest_dir.join("assets").join("icon.ico");
    let icon = icon.display().to_string().replace('\\', "\\\\");
    let template =
        std::fs::read_to_string(manifest_dir.join("assets/app.rc")).expect("read assets/app.rc");
    let generated = template
        .replace("@ICON_PATH@", &icon)
        .replace("@FILE_VERSION@", &file_version)
        .replace("@VERSION@", &version);

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let rc = out_dir.join("app.rc");
    std::fs::write(&rc, generated).expect("write generated app.rc");

    match embed_resource::compile(rc, embed_resource::NONE) {
        embed_resource::CompilationResult::Ok => {
            println!("resources embedded from generated app.rc (v{version})");
        }
        embed_resource::CompilationResult::NotAttempted(reason) => {
            println!("cargo:warning=Windows resources not embedded ({reason})");
        }
        embed_resource::CompilationResult::Failed(reason) => {
            println!("cargo:warning=could not embed Windows resources: {reason}");
        }
        embed_resource::CompilationResult::NotWindows => {}
    }
}
