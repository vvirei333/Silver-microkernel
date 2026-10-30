use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=userspace");
    println!("cargo:rerun-if-changed=logo.txt");
    if env::var_os("CARGO_FEATURE_NO_STD").is_none() {
        return;
    }

    // Link the kernel at the platform's load address: QEMU `virt` places DRAM
    // at 0x4000_0000, the tablet's ABL loads the Image at 0xA800_0000. The
    // linker script also defines `__dram_base` for the boot stub and `mmu.rs`.
    let linker = if env::var_os("CARGO_FEATURE_TABLET").is_some() {
        "linker_tablet.ld"
    } else {
        "linker.ld"
    };
    println!("cargo:rustc-link-arg=-T{linker}");

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let profile = env::var("PROFILE").unwrap();
    let target_dir = root.join("target").join("userspace");
    let cargo = env::var_os("CARGO").unwrap();
    let mut command = Command::new(cargo);
    // The kernel's RUSTFLAGS (linker script selection) must NOT leak into the
    // userspace sub-build: userspace has its own `-Tlinker.ld` in
    // userspace/.cargo/config.toml, and an inherited CARGO_ENCODED_RUSTFLAGS
    // would override it and link the user binaries at the wrong address.
    command
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .current_dir(root.join("userspace"))
        .arg("build")
        .arg("--target")
        .arg("aarch64-unknown-none")
        .arg("--target-dir")
        .arg(&target_dir);
    if profile == "release" {
        command.arg("--release");
    }
    // User ELFs must link at the same base the kernel validates against
    // (`elf::USER_IMAGE_VA`): tablet kernel → tablet user images at 0x8000_0000.
    if env::var_os("CARGO_FEATURE_TABLET").is_some() {
        command.arg("--features").arg("tablet");
    }
    let status = command
        .status()
        .expect("failed to invoke userspace cargo build");
    assert!(status.success(), "userspace ELF build failed");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    for (binary, embedded) in [
        ("silver_userspace", "silver_userspace.elf"),
        ("silverd", "silverd.elf"),
        ("scout", "scout.elf"),
        ("silvershell", "silvershell.elf"),
        ("ls", "ls.elf"),
        ("cat", "cat.elf"),
        ("echo", "echo.elf"),
        ("say", "say.elf"),
        ("sl", "sl.elf"),
        ("uart_driver", "uart_driver.elf"),
        ("block_driver", "block_driver.elf"),
        ("sfs_server", "sfs_server.elf"), // <--- ДОБАВЛЕНО
        ("net_driver", "net_driver.elf"),
        ("net_server", "net_server.elf"),
        ("gpu_driver", "gpu_driver.elf"),
        ("input_driver", "input_driver.elf"),
        // Seeded into the VFS (see src/seed.rs) so `run bin/hello` exercises the
        // dynamic ELF loader and `run bin/crash` exercises EL0 fault containment;
        // deliberately NOT in user_program::image.
        ("hello", "hello.elf"),
        ("crash", "crash.elf"),
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
