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
    let linker = if env::var_os("CARGO_FEATURE_FOG").is_some() {
        "linker_fog.ld"
    } else if env::var_os("CARGO_FEATURE_TABLET").is_some() {
        "linker_tablet.ld"
    } else {
        "linker.ld"
    };
    println!("cargo:rustc-link-arg=-T{linker}");

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());

    // Bundle signing key. `SILVER_BUNDLE_PUBKEY` wins; otherwise a checked-in
    // `keys/bundle_ed25519.pub` (64 hex characters) is used. No key means the
    // kernel stays in integrity-only mode, so default builds keep working.
    println!("cargo:rerun-if-changed=keys/bundle_ed25519.pub");
    println!("cargo:rerun-if-env-changed=SILVER_BUNDLE_PUBKEY");
    let key = env::var("SILVER_BUNDLE_PUBKEY")
        .ok()
        .or_else(|| fs::read_to_string(root.join("keys").join("bundle_ed25519.pub")).ok());
    if let Some(text) = key {
        let text = text.trim();
        assert!(
            text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "bundle public key must be 64 hex characters"
        );
        println!("cargo:rustc-env=SILVER_BUNDLE_PUBKEY={text}");
    }
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
    // Always release: a debug-profile userspace ELF carries ~700 KiB of
    // debug info and overflows the 1 MiB kernel heap at VFS seed time.
    {
        let _ = profile;
        command.arg("--release");
    }
    // User ELFs must link at the same base the kernel validates against
    // (`elf::USER_IMAGE_VA`): tablet kernel → tablet user images at 0x8000_0000.
    if env::var_os("CARGO_FEATURE_TABLET").is_some() {
        command.arg("--features").arg("tablet");
    }
    // The Linux ABI is an optional module: when the kernel feature is off, the
    // shell must not expose `linuxrun` or its syscall wrapper.
    if env::var_os("CARGO_FEATURE_LINUX_ABI").is_some() {
        command.arg("--features").arg("linux-abi");
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
        ("utils", "utils.elf"),
        ("wolfser", "wolfser.elf"),
        ("uart_driver", "uart_driver.elf"),
        ("net_driver", "net_driver.elf"),
        ("net_server", "net_server.elf"),
        // Seeded into the VFS (see src/seed.rs) so `run bin/hello` exercises the
        // dynamic ELF loader and `run bin/crash` exercises EL0 fault containment;
        // deliberately NOT in user_program::image.
        ("bigdir", "bigdir.elf"),
        ("bigfile", "bigfile.elf"),
        ("writeprobe", "writeprobe.elf"),
        ("regprobe", "regprobe.elf"),
        ("scopeprobe", "scopeprobe.elf"),
        ("hello", "hello.elf"),
        ("crash", "crash.elf"),
    ] {
        // The userspace sub-build above is always `--release`, regardless of
        // the kernel profile. Copy from `release/`, not `PROFILE`: a debug
        // kernel used to embed a stale debug userspace ELF (1+ MiB, wrong
        // dispatch) whenever one happened to exist.
        let elf = target_dir
            .join("aarch64-unknown-none")
            .join("release")
            .join(binary);
        let out = out_dir.join(embedded);
        fs::copy(&elf, &out).unwrap_or_else(|error| {
            panic!("failed to embed {}: {error}", elf.display());
        });
    }
}
