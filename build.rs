//! Builds the built-in sync plugins (`plugins/*`) into WebAssembly
//! components, which `src/plugins` embeds.

use std::env;
use std::path::PathBuf;
use std::process::Command;

const PLUGINS: &[&str] = &["s3"];
const TARGET: &str = "wasm32-wasip2";

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap()).join("plugins");
    println!("cargo:rerun-if-changed=wit");

    for plugin in PLUGINS {
        let dir = root.join("plugins").join(plugin);
        println!("cargo:rerun-if-changed=plugins/{plugin}/src");
        println!("cargo:rerun-if-changed=plugins/{plugin}/Cargo.toml");
        println!("cargo:rerun-if-changed=plugins/{plugin}/Cargo.lock");

        let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut command = Command::new(cargo);
        command
            .args(["build", "--release", "--locked", "--target", TARGET])
            .arg("--manifest-path")
            .arg(dir.join("Cargo.toml"))
            // Its own target folder, so it doesn't wait on the app's build.
            .arg("--target-dir")
            .arg(out_dir.join("target"));
        // Settings for the app's build that mustn't leak into the plugin's.
        for var in [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_TARGET",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_RUSTFLAGS",
            "CARGO_PROFILE_RELEASE_DEBUG",
        ] {
            command.env_remove(var);
        }
        let status = command.status().expect("running cargo to build a plugin");
        if !status.success() {
            panic!(
                "building the {plugin} plugin failed (is the {TARGET} target installed? \
                 `rustup target add {TARGET}`)"
            );
        }

        let name = plugin_crate(plugin).replace('-', "_");
        let wasm = out_dir
            .join("target")
            .join(TARGET)
            .join("release")
            .join(format!("{name}.wasm"));
        std::fs::copy(&wasm, out_dir.join(format!("{plugin}.wasm")))
            .unwrap_or_else(|e| panic!("copying {}: {e}", wasm.display()));
    }
}

/// The crate name of the plugin in `plugins/<plugin>`.
fn plugin_crate(plugin: &str) -> String {
    format!("{plugin}-sync")
}
