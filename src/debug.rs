//! ARMv8 hardware watchpoints for the GDB stub.
//!
//! One write watchpoint (DBGWCR0/DBGWVR0) plus the debug-exception enable in
//! MDSCR_EL1. The kernel has no per-task watcher bookkeeping: a watchpoint is
//! a debugging aid armed by the native shell, and the first write anywhere on
//! the machine reports the writer. That is exactly what the phoc corruption
//! needs: arm the field, boot the guest, and catch the store.

use core::sync::atomic::{AtomicU64, Ordering};

static ARMED: AtomicU64 = AtomicU64::new(0);
/// Software watch: the containing page is made read-only, so the first write
/// faults into `cow_fault` even on cores (QEMU TCG) that ignore DBGWCR.
static SOFT_ADDRESS: AtomicU64 = AtomicU64::new(0);
static SOFT_LENGTH: AtomicU64 = AtomicU64::new(0);
static SOFT_HIT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// Log mode: every write to the watched page is reported to the console and
/// allowed through, and the page is re-protected on the next SVC. This is how
/// a page-granular watch finds one guilty store without killing the writer on
/// the first legitimate access.
static SOFT_LOG_MODE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// Pending re-protect captured when a logged hit was allowed through.
static REPROTECT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static REPROTECT_TTBR0: AtomicU64 = AtomicU64::new(0);
static REPROTECT_PAGE: AtomicU64 = AtomicU64::new(0);
static REPROTECT_PHYS: AtomicU64 = AtomicU64::new(0);
static REPROTECT_PROT: AtomicU64 = AtomicU64::new(0);
/// `far`/`elr` of the fault currently being handled, captured by the EL0
/// fault path so `log_soft_hit` can report the exact store address and PC.
static FAULT_FAR: AtomicU64 = AtomicU64::new(0);
static FAULT_ELR: AtomicU64 = AtomicU64::new(0);
static FAULT_VALID: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

const EC_WATCHPOINT_LOW: u64 = 0x34;

fn write_wcr(value: u64) {
    unsafe {
        core::arch::asm!("msr dbgwcr0_el1, {}", in(reg) value, options(nostack));
    }
}

fn write_wvr(value: u64) {
    unsafe {
        core::arch::asm!("msr dbgwvr0_el1, {}", in(reg) value, options(nostack));
    }
    unsafe {
        core::arch::asm!("isb", options(nostack));
    }
}

fn enable_monitors(enable: bool) {
    unsafe {
        let mut mdscr: u64;
        core::arch::asm!("mrs {}, mdscr_el1", out(reg) mdscr, options(nostack));
        if enable {
            // MDE (15): breakpoint/watchpoint exceptions, KDE (13): EL1 debug.
            mdscr |= (1 << 15) | (1 << 13);
        } else {
            mdscr &= !(1 << 15);
        }
        core::arch::asm!("msr mdscr_el1, {}", in(reg) mdscr, options(nostack));
        core::arch::asm!("isb", options(nostack));
    }
}

/// Watches `length` bytes at `address` for writes. Length 1..=8 and the range
/// is rounded to the containing 8-byte granule, which is what DBGWVR/BAS give.
pub fn arm_write(address: u64, length: u64) -> bool {
    if length == 0 || length > 8 || address < 0x1000 || address >= 0x0000_8000_0000_0000 {
        return false;
    }
    let start = (address & !7) as usize;
    let first = (address & 7) as u32;
    let last = (first as u64 + length - 1).min(7) as u32;
    let mut bas = 0u64;
    for bit in first..=last {
        bas |= 1 << bit;
    }
    // E=1, PAC=0, LSC=0b10 (store), BAS, everything else 0.
    let wcr = 1 | (0b10 << 3) | (bas << 5);
    write_wvr(start as u64);
    write_wcr(wcr);
    enable_monitors(true);
    let (read_wcr, read_wvr): (u64, u64);
    unsafe {
        core::arch::asm!("mrs {}, dbgwcr0_el1", out(reg) read_wcr, options(nostack));
        core::arch::asm!("mrs {}, dbgwvr0_el1", out(reg) read_wvr, options(nostack));
    }
    if read_wcr != wcr || read_wvr != start as u64 {
        disarm();
        return false;
    }
    ARMED.store(address, Ordering::Release);
    true
}

pub fn disarm() {
    write_wcr(0);
    write_wvr(0);
    enable_monitors(false);
    ARMED.store(0, Ordering::Release);
    SOFT_LOG_MODE.store(false, Ordering::Release);
    REPROTECT.store(false, Ordering::Release);
    SOFT_LENGTH.store(0, Ordering::Release);
}

pub fn armed() -> u64 {
    ARMED.load(Ordering::Acquire)
}

/// Arms the software watch. The page is protected lazily: `mmap_region` and
/// `lazy_page_fault` call [`soft_page_is`] and clear the write bit.
pub fn arm_soft(address: u64, length: u64) -> bool {
    if length == 0 || length > 4096 || address < 0x1000 || address >= 0x0000_8000_0000_0000 {
        return false;
    }
    SOFT_ADDRESS.store(address, Ordering::Release);
    SOFT_LENGTH.store(length, Ordering::Release);
    SOFT_HIT.store(false, Ordering::Release);
    true
}

pub fn soft_armed() -> bool {
    SOFT_LENGTH.load(Ordering::Acquire) != 0
}

pub fn soft_page() -> u64 {
    SOFT_ADDRESS.load(Ordering::Acquire) & !0xfff
}

pub fn soft_page_is(page: u64) -> bool {
    soft_armed() && page == soft_page()
}

pub fn soft_contains(address: u64) -> bool {
    let start = SOFT_ADDRESS.load(Ordering::Acquire);
    let length = SOFT_LENGTH.load(Ordering::Acquire);
    address >= start && address < start.saturating_add(length)
}

pub fn note_soft_hit() {
    SOFT_HIT.store(true, Ordering::Release);
}

pub fn take_soft_hit() -> bool {
    SOFT_HIT.swap(false, Ordering::Acquire)
}

/// Arms the software page watch in log mode: `length` is expected to cover a
/// page (4096), every write to that page is reported and allowed through.
pub fn arm_soft_log(address: u64, length: u64) -> bool {
    if !arm_soft(address, length) {
        return false;
    }
    SOFT_LOG_MODE.store(true, Ordering::Release);
    true
}

pub fn log_mode() -> bool {
    SOFT_LOG_MODE.load(Ordering::Acquire)
}

/// Captures the fault context for `log_soft_hit` (called from the EL0 fault
/// path before the scheduler repairs the mapping).
pub fn set_fault_context(far: u64, elr: u64) {
    FAULT_FAR.store(far, Ordering::Release);
    FAULT_ELR.store(elr, Ordering::Release);
    FAULT_VALID.store(true, Ordering::Release);
}

pub fn clear_fault_context() {
    FAULT_VALID.store(false, Ordering::Release);
}

/// True when the current `cow_fault` came from a real EL0 fault whose `far`
/// and `elr` were captured.
pub fn fault_context_valid() -> bool {
    FAULT_VALID.load(Ordering::Acquire)
}

/// Reports one write to the watched page as a single console line: the store
/// address (`far`), the PC (`elr`) and whether it hit the exact watched range.
pub fn log_soft_hit(exact: bool) {
    let far = FAULT_FAR.load(Ordering::Acquire);
    let elr = FAULT_ELR.load(Ordering::Acquire);
    let mut buffer = [0u8; 96];
    let header: &[u8] = if exact {
        b"WATCH* far="
    } else {
        b"WATCH  far="
    };
    buffer[..header.len()].copy_from_slice(header);
    let mut length = header.len();
    for (value, prefix) in [(far, None), (elr, Some(b" elr="))] {
        if let Some(text) = prefix {
            buffer[length..length + text.len()].copy_from_slice(text);
            length += text.len();
        }
        for position in 0..16 {
            let nibble = ((value >> (60 - 4 * position)) & 0xF) as u8;
            buffer[length] = if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + nibble - 10
            };
            length += 1;
        }
    }
    buffer[length] = b'\r';
    buffer[length + 1] = b'\n';
    crate::console::write_bytes(&buffer[..length + 2]);
}

/// Remembers the mapping a logged hit was allowed through, so the next SVC
/// can restore the read-only protection. `prot` is the region protection the
/// page was mapped with (writable here).
pub fn request_reprotect(ttbr0: u64, page: u64, physical: u64, prot: u64) {
    REPROTECT_TTBR0.store(ttbr0, Ordering::Release);
    REPROTECT_PAGE.store(page, Ordering::Release);
    REPROTECT_PHYS.store(physical, Ordering::Release);
    REPROTECT_PROT.store(prot, Ordering::Release);
    REPROTECT.store(true, Ordering::Release);
}

/// Called at SVC entry: if a logged hit was allowed through earlier, make the
/// watched page read-only again. The faulting store has retired by now.
#[cfg(feature = "gdb-stub")]
pub fn reprotect_soft_page() {
    if !REPROTECT.swap(false, Ordering::Acquire) {
        return;
    }
    let ttbr0 = REPROTECT_TTBR0.load(Ordering::Acquire);
    let page = REPROTECT_PAGE.load(Ordering::Acquire);
    let physical = REPROTECT_PHYS.load(Ordering::Acquire);
    let prot = REPROTECT_PROT.load(Ordering::Acquire);
    let _ = crate::mmu::map_user_page_at(
        ttbr0 as usize,
        page as usize,
        physical as usize,
        prot & !crate::mmu::PROT_WRITE,
    );
}

pub fn is_watchpoint(esr: u64) -> bool {
    ((esr >> 26) & 0x3f) == EC_WATCHPOINT_LOW
}
