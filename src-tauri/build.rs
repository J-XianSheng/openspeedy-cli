//! Build script - compile 32/64-bit speedpatch DLLs and bridge EXEs.
//!
//! - speedpatch DLLs: CMake + MinHook (C++)
//! - bridge EXEs: cargo build of the `src-bridge` crate (host arch + i686)
//!
//! Both are Windows-only native artifacts. When they cannot or should not be
//! produced (non-Windows host, or a type-check pass), set
//! `OPENSPEEDY_SKIP_NATIVE_BUILD=1` to skip the pipeline — the Rust code still
//! type-checks against the Windows API surface.
//!
//! - Host arch: cmake crate auto-detects the compiler
//! - Cross arch: cmake -A to target the other platform

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build a cmake project (host arch) via the cmake crate, copy the output binary.
fn build_cmake_crate(
    manifest_dir: &Path,
    profile: &str,
    project_dir: &str,    // relative to manifest_dir, e.g. "../speedpatch"
    bin_prefix: &str,     // e.g. "speedpatch" or "bridge"
    bin_suffix: &str,     // "32" or "64"
    file_ext: &str,       // "dll" or "exe"
) {
    let build_type = if profile == "release" { "Release" } else { "Debug" };
    let bin_name = format!("{}{}.{}", bin_prefix, bin_suffix, file_ext);

    println!("cargo:info=[cmake crate] building {bin_name} (host arch)...");

    let dst = cmake::Config::new(project_dir)
        .define("CMAKE_BUILD_TYPE", build_type)
        .build();

    let bin_src = dst.join("bin").join(&bin_name);
    let prof_dir = manifest_dir.join("target").join(profile);
    let bin_dst = prof_dir.join(&bin_name);

    std::fs::copy(&bin_src, &bin_dst).unwrap_or_else(|e| {
        panic!("copy {} -> {} failed: {}", bin_src.display(), bin_dst.display(), e);
    });
    println!("cargo:info=  -> {}", bin_dst.display());
}

/// Build a cmake project (cross arch) via cmake command line, copy the output binary.
#[allow(clippy::too_many_arguments)]
fn build_cmake_command(
    manifest_dir: &Path,
    profile: &str,
    project_dir: &str,       // relative to manifest_dir, e.g. "../speedpatch"
    cmake_arch: &str,         // "Win32" or "x64"
    bin_prefix: &str,         // e.g. "speedpatch" or "bridge"
    bin_suffix: &str,         // "32" or "64"
    file_ext: &str,           // "dll" or "exe"
    build_subdir: &str,       // subdirectory under target/ for build artifacts
) {
    let build_type = if profile == "release" { "Release" } else { "Debug" };
    let bin_name = format!("{}{}.{}", bin_prefix, bin_suffix, file_ext);
    let build_dir = manifest_dir.join("target").join(build_subdir).join(cmake_arch);

    println!("cargo:info=[cmake cmd] building {bin_name} (-A {cmake_arch})...");

    let source_dir = manifest_dir.join(project_dir);
    let status = Command::new("cmake")
        .args([
            "-S", &source_dir.to_string_lossy(),
            "-B", &build_dir.to_string_lossy(),
            "-A", cmake_arch,
        ])
        .status()
        .expect("cmake configure failed");

    if !status.success() {
        panic!("cmake configure returned error for {}", bin_name);
    }

    let status = Command::new("cmake")
        .args(["--build", &build_dir.to_string_lossy(), "--config", build_type])
        .status()
        .expect("cmake build failed");

    if !status.success() {
        panic!("cmake build returned error for {}", bin_name);
    }

    let bin_src = build_dir.join(build_type).join(&bin_name);
    let prof_dir = manifest_dir.join("target").join(profile);
    let bin_dst = prof_dir.join(&bin_name);

    std::fs::copy(&bin_src, &bin_dst).unwrap_or_else(|e| {
        panic!("copy {} -> {} failed: {}", bin_src.display(), bin_dst.display(), e);
    });
    println!("cargo:info=  -> {}", bin_dst.display());
}

/// Build both architectures of a project.
fn build_project(
    manifest_dir: &Path,
    profile: &str,
    target_arch: &str,
    project_dir: &str,
    bin_prefix: &str,
    file_ext: &str,
    build_subdir: &str,
) {
    match target_arch {
        "x86_64" => {
            build_cmake_crate(manifest_dir, profile, project_dir, bin_prefix, "64", file_ext);
            build_cmake_command(manifest_dir, profile, project_dir, "Win32", bin_prefix, "32", file_ext, build_subdir);
        }
        "x86" => {
            build_cmake_crate(manifest_dir, profile, project_dir, bin_prefix, "32", file_ext);
            build_cmake_command(manifest_dir, profile, project_dir, "x64", bin_prefix, "64", file_ext, build_subdir);
        }
        _ => unreachable!(),
    }
}

/// Build the Rust bridge crate via cargo for one target triple.
/// Returns `(target_arch_name, built_exe_path)` on success.
fn cargo_build_bridge(bridge_dir: &Path, profile: &str, target: Option<&str>) -> Option<(String, PathBuf)> {
    let profile_flag = if profile == "release" { "--release" } else { "" };
    let build_type = if profile == "release" { "release" } else { "debug" };

    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--manifest-path", &bridge_dir.join("Cargo.toml").to_string_lossy()]);
    if !profile_flag.is_empty() { cmd.arg(profile_flag); }
    if let Some(t) = target { cmd.args(["--target", t]); }

    let label = target.unwrap_or("host");
    println!("cargo:info=[cargo] building bridge ({label})...");
    let status = cmd.status().expect("cargo build bridge failed");
    if !status.success() {
        println!("cargo:warning=cargo build bridge ({label}) returned error");
        return None;
    }

    let target_dir = if let Some(t) = target {
        bridge_dir.join("target").join(t).join(build_type)
    } else {
        bridge_dir.join("target").join(build_type)
    };
    let src = target_dir.join("bridge.exe");
    if !src.exists() { return None; }
    Some((label.to_string(), src))
}

/// Build Rust bridge for host arch (→ bridge64.exe) and optionally i686 (→ bridge32.exe).
fn build_rust_bridge(manifest_dir: &Path, profile: &str) {
    let bridge_dir = manifest_dir.join("../src-bridge");
    let prof_dir = manifest_dir.join("target").join(profile);

    // --- 64-bit (host arch) ---
    if let Some((_, src)) = cargo_build_bridge(&bridge_dir, profile, None) {
        let dst = prof_dir.join("bridge64.exe");
        std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {} -> {} failed: {}", src.display(), dst.display(), e));
        println!("cargo:info=  -> {}", dst.display());
    }

    // --- 32-bit (cross-compile if i686 target installed) ---
    let check = Command::new("rustup").args(["target", "list", "--installed"]).output();
    let has_i686 = check.map(|o| {
        String::from_utf8_lossy(&o.stdout).contains("i686-pc-windows-msvc")
    }).unwrap_or(false);

    if has_i686 {
        if let Some((_, src)) = cargo_build_bridge(&bridge_dir, profile, Some("i686-pc-windows-msvc")) {
            let dst = prof_dir.join("bridge32.exe");
            std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {} -> {} failed: {}", src.display(), dst.display(), e));
            println!("cargo:info=  -> {}", dst.display());
        }
    } else {
        println!("cargo:warning=i686-pc-windows-msvc target not installed — skipping bridge32.exe");
        println!("cargo:warning=Install with: rustup target add i686-pc-windows-msvc");
    }
}

fn main() {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();

    // speedpatch DLLs and bridge EXEs are Windows native artifacts; nothing can
    // (or should) be produced for other targets.
    if target_os != "windows" {
        println!("cargo:warning=target is not Windows — skipping speedpatch/bridge native build");
        return;
    }

    // Escape hatch for type-checking the Rust side without the native toolchain
    // (e.g. `OPENSPEEDY_SKIP_NATIVE_BUILD=1 cargo check --target x86_64-pc-windows-msvc`
    // on a non-Windows host).
    if env::var("OPENSPEEDY_SKIP_NATIVE_BUILD").is_ok() {
        println!("cargo:warning=OPENSPEEDY_SKIP_NATIVE_BUILD is set — skipping speedpatch/bridge native build");
        return;
    }

    match target_arch.as_str() {
        "x86" | "x86_64" => {}
        _ => {
            println!("cargo:warning=speedpatch/bridge only support x86/x86_64 Windows — skipping native build");
            return;
        }
    }

    let profile = env::var("PROFILE").unwrap();
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // --- speedpatch DLLs (C++ / cmake) ---
    build_project(&manifest_dir, &profile, target_arch.as_str(),
        "../src-bridge/speedpatch", "speedpatch", "dll", "speedpatch-build");

    // --- bridge EXEs (Rust / cargo) ---
    build_rust_bridge(&manifest_dir, &profile);

    println!("cargo:rerun-if-changed=../src-bridge/speedpatch/");
    println!("cargo:rerun-if-changed=../src-bridge/third_party/minhook/");
    println!("cargo:rerun-if-changed=../src-bridge/src/");
}
