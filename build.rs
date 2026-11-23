use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=userspace");
    // One source of truth for the release version: Cargo.toml. Modules use
    // `env!("SILVER_VERSION")`, and the userspace sub-build receives the same
    // value, so the kernel banner, `uname` and `utsname` can never disagree.
    let version = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    println!("cargo:rustc-env=SILVER_VERSION={version}");
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

    // Placement override for a board bootloader. The linker script keeps the
    // QEMU defaults and only uses these when `--defsym` defines them, so a
    // default build is unchanged. `SILVER_DRAM_BASE` is where the loader puts
    // DRAM (the MMU's identity alias and the boot stub read it); `SILVER_LINK_PA`
    // is where the image is loaded; `SILVER_KERNEL_VIRT_BASE` is the high-half
    // window and should not change in practice.
    for (var, sym) in [
        ("SILVER_LINK_PA", "__silver_link_pa"),
        ("SILVER_DRAM_BASE", "__silver_dram_base"),
        ("SILVER_KERNEL_VIRT_BASE", "__silver_kernel_virt_base"),
    ] {
        println!("cargo:rerun-if-env-changed={var}");
        let Ok(raw) = env::var(var) else { continue };
        let text = raw.trim();
        let value = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => text.parse::<u64>(),
        }
        .unwrap_or_else(|_| panic!("{var} must be a decimal or 0x-prefixed u64, got {raw:?}"));
        assert!(
            value % 0x1000 == 0,
            "{var}=0x{value:x} is not page aligned (the image is mapped by page)"
        );
        println!("cargo:rustc-link-arg=--defsym={sym}=0x{value:x}");
    }

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
        .env("SILVER_VERSION", &version)
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
        ("secprobe", "secprobe.elf"),
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
