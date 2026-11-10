//! Builds the built-in plugins (`plugins/*`) into WebAssembly components,
//! which `src/plugins` embeds.

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Each plugin's folder in `plugins`, and its crate's name.
const PLUGINS: &[(&str, &str)] = &[("s3", "s3-sync"), ("openai-vision", "openai-vision-detect")];
const TARGET: &str = "wasm32-wasip2";

fn main() {
    link_ffmpeg();
    link_libheif();
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap()).join("plugins");
    println!("cargo:rerun-if-changed=wit");

    for (plugin, crate_name) in PLUGINS {
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

        let name = crate_name.replace('-', "_");
        let wasm = out_dir
            .join("target")
            .join(TARGET)
            .join("release")
            .join(format!("{name}.wasm"));
        std::fs::copy(&wasm, out_dir.join(format!("{plugin}.wasm")))
            .unwrap_or_else(|e| panic!("copying {}: {e}", wasm.display()));
    }
}

/// With FFMPEG_DIR set, as to the LGPL build from `scripts/build-ffmpeg.sh`,
/// ffmpeg-sys links against the libraries there. They're named
/// `@rpath/libav*.dylib`, so the app looks for them along its rpath: here,
/// that folder, so it runs from `target/`. A bundled app adds its own
/// Frameworks folder instead. Without FFMPEG_DIR, FFmpeg comes from
/// pkg-config (Homebrew's, for development), whose libraries have
/// absolute names and need no rpath.
fn link_ffmpeg() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    if let Ok(dir) = env::var("FFMPEG_DIR") {
        let lib = PathBuf::from(dir).join("lib");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
    }
}

/// libheif, found through pkg-config, likewise: if it's the build from
/// `scripts/build-libheif.sh` (put first on PKG_CONFIG_PATH), named
/// `@rpath/libheif.1.dylib`, its folder goes on the rpath too. Homebrew's
/// has an absolute name and needs nothing.
fn link_libheif() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let Ok(output) = Command::new("pkg-config")
        .args(["--variable=libdir", "libheif"])
        .output()
    else {
        return;
    };
    let libdir = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if libdir.is_empty() {
        return;
    }
    let id = Command::new("otool")
        .arg("-D")
        .arg(PathBuf::from(&libdir).join("libheif.dylib"))
        .output();
    if id.is_ok_and(|id| String::from_utf8_lossy(&id.stdout).contains("@rpath/")) {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{libdir}");
    }
}
