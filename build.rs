use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let rc = PathBuf::from("assets/app.rc");
    match embed_resource::compile(rc, embed_resource::NONE) {
        embed_resource::CompilationResult::Ok => {
            println!("resources embedded from assets/app.rc");
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
