use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Locate the xPack aarch64-none-elf toolchain (newlib + libstdc++) that
/// builds the litehtml engine. It lives outside the repository because it is
/// ~440 MiB; `WOLF_ARM_TOOLCHAIN` overrides the search.
fn find_toolchain() -> Option<PathBuf> {
    if let Ok(path) = env::var("WOLF_ARM_TOOLCHAIN") {
        let path = PathBuf::from(path);
        if path.join("bin/aarch64-none-elf-g++").exists() {
            return Some(path);
        }
    }
    let home = env::var("HOME").ok()?;
    let base = PathBuf::from(home).join(".local/toolchains");
    for entry in std::fs::read_dir(base).ok()? {
        let path = entry.ok()?.path();
        if path.join("bin/aarch64-none-elf-g++").exists() {
            return Some(path);
        }
    }
    None
}

fn library_path(compiler: &Path, name: &str) -> Option<PathBuf> {
    let output = Command::new(compiler)
        .arg(format!("-print-file-name={name}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    if path.is_absolute() && path.exists() {
        Some(path)
    } else {
        None
    }
}

fn build_engine(root: &Path, out: &Path, toolchain: &Path) {
    println!("cargo:rerun-if-changed=engine/build.sh");
    println!("cargo:rerun-if-changed=engine/wolf_container.cpp");
    println!("cargo:rerun-if-changed=engine/wolf_container.h");
    println!("cargo:rerun-if-changed=engine/wolf_html.cpp");
    println!("cargo:rerun-if-changed=engine/wolf_syscalls.c");
    println!("cargo:rerun-if-changed=../third_party/litehtml/src");
    println!("cargo:rerun-if-changed=../third_party/litehtml/include");

    let status = Command::new("bash")
        .arg(root.join("engine/build.sh"))
        .arg(out)
        .env("WOLF_ARM_TOOLCHAIN", toolchain)
        .status()
        .expect("failed to run engine/build.sh");
    assert!(status.success(), "litehtml engine build failed");

    let compiler = toolchain.join("bin/aarch64-none-elf-g++");
    // litehtml mentions one dynamic_cast, so RTTI is on; exceptions are on
    // only for the charset prescan and the parse wrapper. libstdc++ and newlib
    // provide the remaining runtime.
    println!("cargo:rustc-link-arg-bin=wolfser=--start-group");
    println!(
        "cargo:rustc-link-arg-bin=wolfser={}",
        out.join("libwolfhtml.a").display()
    );
    for name in ["libstdc++.a", "libm.a", "libc.a", "libgcc.a"] {
        let path = library_path(&compiler, name)
            .unwrap_or_else(|| panic!("aarch64-none-elf toolchain is missing {name}"));
        println!("cargo:rustc-link-arg-bin=wolfser={}", path.display());
    }
    println!("cargo:rustc-link-arg-bin=wolfser=--end-group");
}

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=linker_tablet.ld");
    // The user image base must match the kernel's `elf::USER_IMAGE_VA` for the
    // same feature: QEMU DRAM starts at 0x4000_0000 (linker.ld), the tablet's
    // at 0x8000_0000 (linker_tablet.ld). The kernel passes `--features tablet`
    // down to this sub-build (root build.rs) when it is itself built as tablet.
    let linker = if env::var_os("CARGO_FEATURE_TABLET").is_some() {
        "linker_tablet.ld"
    } else {
        "linker.ld"
    };
    println!("cargo:rustc-link-arg=-T{linker}");

    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_arch != "aarch64" || target_os != "none" {
        return;
    }

    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("wolfhtml");
    let Some(toolchain) = find_toolchain() else {
        panic!(
            "aarch64-none-elf toolchain not found; install xPack AArch64 Embedded GCC \
             or set WOLF_ARM_TOOLCHAIN (see userspace/engine/README.md)"
        );
    };
    build_engine(&root, &out, &toolchain);
}
