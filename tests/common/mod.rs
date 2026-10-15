//! Helpers shared by the end-to-end tests.

#![allow(dead_code, reason = "not all helpers are used by every test")]

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn wasmtime() -> Option<PathBuf> {
    let p = std::env::var_os("WASMTIME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../wasmtime/target/debug/wasmtime")
        });
    if p.exists() {
        Some(p)
    } else {
        eprintln!("skipping: no wasmtime at {p:?} (set WASMTIME)");
        None
    }
}

pub fn programs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs")
}

pub fn out_dir() -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join("programs");
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn build_c(name: &str) -> Option<PathBuf> {
    let Some(sdk) = std::env::var_os("WASI_SDK_PATH") else {
        eprintln!("skipping: WASI_SDK_PATH not set");
        return None;
    };
    let out = out_dir().join(format!("{name}.wasm"));
    let status = Command::new(Path::new(&sdk).join("bin/wasm32-wasip1-clang"))
        .arg("-g")
        .arg(programs().join(format!("{name}.c")))
        .arg("-o")
        .arg(&out)
        .env("PATH", "")
        .status()
        .unwrap();
    assert!(status.success());
    Some(out)
}

pub fn build_rust(name: &str) -> Option<PathBuf> {
    let out = out_dir().join(format!("{name}.wasm"));
    let status = Command::new("rustc")
        .args(["--edition", "2024", "--target", "wasm32-wasip1", "-g"])
        .arg(programs().join(format!("{name}.rs")))
        .arg("-o")
        .arg(&out)
        .status()
        .ok()?;
    if !status.success() {
        eprintln!("skipping: cannot build Rust test program (wasm32-wasip1 target installed?)");
        return None;
    }
    Some(out)
}
