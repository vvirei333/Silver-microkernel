//! Runtime platform identity (hardware half): reads `MIDR_EL1` and applies
//! the identity the device tree proves. The enum and the matcher live in
//! `src/board.rs` so they can be unit-tested on the host; this module keeps
//! only what needs a system register.
//!
//! The boot path needs one source of truth for "where am I?" because UART,
//! GIC, DRAM and the kernel load address differ per target. Two independent
//! signals are used:
//!
//! * the live FDT root `compatible` (authoritative when a DTB was passed),
//! * `MIDR_EL1` as a fallback for the two emulated/reference targets.
//!
//! An unrecognised CPU is **not** silently treated as the tablet any more:
//! that used to route MMIO to SM8650 addresses on any unknown AArch64 part,
//! which is a bus abort waiting to happen. `Kind::Unknown` makes the console
//! and interrupt paths fail closed instead.

/// Identity type shared with the kernel library (`src/board.rs`).
///
/// The enum, the `compatible` matcher and the cached answer live in the
/// library so `cargo test --lib` can cover them; this module adds the only
/// part that needs hardware: reading `MIDR_EL1`.
pub use silver_microkernel::board::Kind;
pub use silver_microkernel::board::{kind_from_compatible, force_from_compatible};

/// Records the identity proved by the live FDT. Called once, before the
/// console is initialised.
pub fn force(kind: Kind) {
    silver_microkernel::board::force(kind);
}

/// Returns the cached identity, detecting it from `MIDR_EL1` on first use.
pub fn detect() -> Kind {
    if let Some(kind) = silver_microkernel::board::cached() {
        return kind;
    }

    let part = ((midr_el1() >> 4) & 0xFFF) as u32;
    let kind = silver_microkernel::board::kind_from_midr_part(part);
    silver_microkernel::board::force(kind);
    kind
}

fn midr_el1() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, midr_el1", out(reg) value, options(nomem, nostack));
    }
    value
}
