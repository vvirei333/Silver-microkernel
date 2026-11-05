//! Data and instruction cache enable.
//!
//! The boot stub builds the translation tables and turns the MMU on with
//! `SCTLR_EL1.C = 0` and `I = 0`: every access is plain memory and the stub
//! needs no maintenance. Once the kernel executes from the high half and the
//! page tables are published, caches can be turned on. This module owns that
//! transition and records, per CPU, the `SCTLR_EL1` value each CPU actually
//! read back, so the boot self-test can prove the bits took effect on every
//! online PE instead of trusting the code path that set them.
//!
//! Turning caches on makes the existing maintenance ritually load-bearing:
//! table edits already clean their lines before the broadcast `tlbi`, and DMA
//! publication already cleans the cacheable alias before the device or EL0 can
//! see the buffer. Before this module existed those loops were correct but
//! mostly ornamental because nothing was cached.

use core::sync::atomic::{AtomicU64, Ordering};

use silver_microkernel::cache_policy;

use crate::smp::MAX_CPUS;

/// `SCTLR_EL1.C` — data cache enable.
pub const SCTLR_C: u64 = 1 << 2;
/// `SCTLR_EL1.I` — instruction cache enable.
pub const SCTLR_I: u64 = 1 << 12;

/// Bisect switch: `false` keeps the pre-cache boot (MMU on, caches off) and
/// makes the boot self-test fail instead of silently skipping the feature.
pub const ENABLE_CACHES: bool = true;

/// `SCTLR_EL1` as read back on each CPU after its enable attempt. Indexed by
/// CPU slot, which is also the AP's slot index and the BSP's `0`.
static CACHE_RECORD: [AtomicU64; MAX_CPUS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

#[inline]
pub fn read_sctlr() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) value, options(nostack));
    }
    value
}

/// Whether both cache-enable bits are set in an `SCTLR_EL1` value.
pub fn caches_on(sctlr: u64) -> bool {
    sctlr & SCTLR_C != 0 && sctlr & SCTLR_I != 0
}

/// The value recorded on `cpu` (0 before that CPU has run the enable path).
pub fn recorded(cpu: usize) -> u64 {
    CACHE_RECORD[cpu.min(MAX_CPUS - 1)].load(Ordering::Acquire)
}

fn record(cpu: usize, value: u64) {
    CACHE_RECORD[cpu.min(MAX_CPUS - 1)].store(value, Ordering::Release);
}

fn read_ctr() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, ctr_el0", out(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Data-cache line size this CPU reports; used by the boot log.
pub fn line_bytes() -> usize {
    cache_policy::dmin_line_bytes(read_ctr())
}

fn read_clidr() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, clidr_el1", out(reg) value, options(nostack));
    }
    value
}

fn read_ccsidr() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, ccsidr_el1", out(reg) value, options(nostack));
    }
    value
}

fn select_cache_level(level: u64) {
    unsafe {
        core::arch::asm!("msr csselr_el1, {}", in(reg) level, options(nostack));
        core::arch::asm!("isb", options(nostack));
    }
}

fn dsb_sy_isb() {
    unsafe {
        core::arch::asm!("dsb sy", "isb", options(nostack));
    }
}

/// Invalidates every data-cache line in every level by set/way.
///
/// Boot-only: set/way maintenance is not broadcast and may discard another
/// PE's dirty lines on a shared cache, so this runs on the boot CPU before any
/// application CPU exists. It removes whatever the firmware left behind; the
/// kernel's own writes so far went to memory because `SCTLR_EL1.C` was clear.
fn invalidate_dcache_all() {
    let clidr = read_clidr();
    for level in 1..=7usize {
        if !cache_policy::level_has_data_cache(clidr, level) {
            continue;
        }
        // CSSELR: bits [3:1] = level - 1, bit 0 = 0 selects the data cache.
        select_cache_level(((level - 1) as u64) << 1);
        let (line_bytes, ways, sets) = cache_policy::ccsidr_geometry(read_ccsidr());
        let line_shift = line_bytes.trailing_zeros();
        let way_shift = 32 - ways.trailing_zeros();
        for way in 0..ways {
            for set in 0..sets {
                let operand = cache_policy::set_way_operand(
                    level - 1,
                    line_shift,
                    way_shift,
                    way,
                    set,
                );
                unsafe {
                    core::arch::asm!("dc isw, {}", in(reg) operand, options(nostack));
                }
            }
        }
    }
    dsb_sy_isb();
}

/// Drops every instruction-cache line after a code change made through the
/// data side. `ialluis` also covers the other PEs in the inner-shareable
/// domain, which matters for the AP's view of code published by the BSP.
fn invalidate_icache_all() {
    unsafe {
        core::arch::asm!("ic ialluis", options(nostack));
    }
    dsb_sy_isb();
}

/// Sets `C` and `I` in one write and returns the value read back.
fn enable_and_read_back() -> u64 {
    let value = read_sctlr() | SCTLR_C | SCTLR_I;
    unsafe {
        core::arch::asm!("msr sctlr_el1, {}", "isb", in(reg) value, options(nostack));
    }
    read_sctlr()
}

/// Boot-CPU entry, called from `mmu::init` after the tables are published and
/// the MMU is on. Invalidates the inherited cache state, then enables both
/// caches. Returns whether the bits read back set.
pub fn init_boot() -> bool {
    if !ENABLE_CACHES {
        record(0, read_sctlr());
        return false;
    }
    invalidate_dcache_all();
    invalidate_icache_all();
    let after = enable_and_read_back();
    record(0, after);
    caches_on(after)
}

/// Application-CPU entry. Must be the first Rust-side action an AP takes: it
/// makes the AP's memory attributes match the boot CPU's before the AP touches
/// any shared kernel state. Touching a cacheable object from a PE that still
/// treats it as non-cacheable is the attribute mismatch the architecture calls
/// unpredictable.
///
/// The AP does not run the boot CPU's set/way invalidation: after `CPU_ON` the
/// PE starts in the architectural reset state and, unlike the boot CPU, it has
/// no earlier cached view of this kernel to discard — while a set/way sweep
/// from a second PE could discard the boot CPU's dirty lines on a shared
/// cache. Re-verifying this assumption on physical hardware is an open HIL
/// item (SMP is QEMU-only today). Returns the `SCTLR_EL1` value read back, for
/// [`record_secondary`].
pub fn enable_secondary() -> u64 {
    if !ENABLE_CACHES {
        return read_sctlr();
    }
    invalidate_icache_all();
    enable_and_read_back()
}

/// Publishes an AP's read-back `SCTLR_EL1` for the boot self-test. Called once
/// the AP knows its slot index and before it goes online.
pub fn record_secondary(cpu: usize, value: u64) -> bool {
    record(cpu, value);
    caches_on(value)
}

/// Boot self-test: every online CPU must have read `C=1 I=1` back from its own
/// `SCTLR_EL1`, and this CPU must still report both bits.
#[cfg(all(feature = "smp", feature = "smp-selftest"))]
pub fn run_cache_selftest() -> bool {
    let local = caches_on(read_sctlr());
    let mut online = 0u64;
    let mut all = local;
    for cpu in 0..MAX_CPUS {
        if !crate::smp::cpu_online(cpu) {
            continue;
        }
        online += 1;
        if !caches_on(recorded(cpu)) {
            all = false;
        }
    }
    if all {
        crate::console::write_str("SMP SELFTEST: cache PASS (");
        crate::console::write_dec_u64(online);
        crate::console::write_str(" CPU(s) C=1 I=1)\r\n");
    } else {
        crate::console::write_str("SMP SELFTEST: cache FAIL (online=");
        crate::console::write_dec_u64(online);
        crate::console::write_str(" local_sctlr=0x");
        crate::console::write_hex_u64(read_sctlr());
        crate::console::write_str(")\r\n");
    }
    all
}
