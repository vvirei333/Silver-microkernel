use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=userspace");
    println!("cargo:rerun-if-changed=logo.txt");
    if env::var_os("CARGO_FEATURE_NO_STD").is_none() {
        return;
    }

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let profile = env::var("PROFILE").unwrap();
    let target_dir = root.join("target").join("userspace");
    let cargo = env::var_os("CARGO").unwrap();
    let mut command = Command::new(cargo);
    command
        .current_dir(root.join("userspace"))
        .arg("build")
        .arg("--target")
        .arg("aarch64-unknown-none")
        .arg("--target-dir")
        .arg(&target_dir);
    if profile == "release" {
        command.arg("--release");
    }
    let status = command
        .status()
        .expect("failed to invoke userspace cargo build");
    assert!(status.success(), "userspace ELF build failed");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    for (binary, embedded) in [
        ("silver_userspace", "silver_userspace.elf"),
        ("nameserver", "silver_nameserver.elf"),
        ("silvershell", "silvershell.elf"),
        ("ls", "ls.elf"),
        ("cat", "cat.elf"),
        ("echo", "echo.elf"),
        ("say", "say.elf"),
        ("sl", "sl.elf"),
    ] {
        let elf = target_dir
            .join("aarch64-unknown-none")
            .join(&profile)
            .join(binary);
        let out = out_dir.join(embedded);
        fs::copy(&elf, &out).unwrap_or_else(|error| {
            panic!("failed to embed {}: {error}", elf.display());
        });
    }
}
