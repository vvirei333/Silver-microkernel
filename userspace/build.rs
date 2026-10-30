use std::env;

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
}
