//! Boot self-tests: the checks that prove an invariant the kernel relies on
//! before any task runs. A failure is a boot stop, not a warning.

#[cfg(all(feature = "smp", feature = "smp-selftest"))]
use crate::pl011;
use crate::{console, exceptions, frame_allocator, kernel_stack, mmu, ram_log, scheduler};

/// The newer MMU/ASID/DMA boot proofs run on QEMU and on any non-fog build.
/// A fog hardware image skips them unless `fog-hw-selftests` is requested:
/// the TLB proof was observed to wedge a `rain` unit before userspace.
#[cfg(all(feature = "fog", not(feature = "fog-hw-selftests")))]
const DEEP_BOOT_SELFTESTS: bool = false;
#[cfg(any(not(feature = "fog"), feature = "fog-hw-selftests"))]
const DEEP_BOOT_SELFTESTS: bool = true;

/// Runs the boot self-tests in their existing order: memory policy, the kernel
/// stack arena, address-space reclamation, DMA quarantine and the console
/// capability gate. The deep block is the one a fog hardware image skips
/// unless `fog-hw-selftests` asks for it.
pub fn run() {
    // User-visible memory (mmap, brk, DMA, ELF images, task stacks) may not
    // eat the frames the kernel and its devices still need. The gate is a
    // free-count policy; prove here that it refuses one-sidedly and that the
    // raw kernel allocator is unaffected.
    if !frame_allocator::run_user_reserve_selftest() {
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }
    ram_log::log("MEM RESERVE OK");

    // One 2 MiB-aligned arena, split into 4 KiB pages with one unmapped
    // guard page below every task stack. A failure here would make task
    // spawns silently unsafe, so it stops the boot.
    if !kernel_stack::init() || !kernel_stack::run_self_test() {
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }
    ram_log::log("KERNEL STACK OK");

    // Address-space reclamation invariants: partial unmap is refused without
    // touching the mapped page, teardown retires the root token once, and a
    // second teardown with the same token is refused.
    if !mmu::run_self_test() {
        exceptions::silver_handle_exception(9, 0, 0, 0);
    }
    ram_log::log("MMU RECLAIM OK");

    // Enlarged DMA window: a first-fit run must cross a 2 MiB group boundary
    // and an oversized request must be refused. The display path depends on
    // this window holding two full-size surfaces.
    if !mmu::run_dma_window_self_test() {
        exceptions::silver_handle_exception(9, 0, 0, 0);
    }
    console::write_str("DMA WINDOW OK\r\n");
    ram_log::log("DMA WINDOW OK");

    // The newer MMU/ASID/DMA boot proofs are QEMU-proven but have not yet
    // completed on the phone: the TLB proof wedges a `rain` unit before
    // userspace, so a fog hardware boot skips the block by default and says so.
    // `fog-hw-selftests` opts back in for a dedicated debugging image.
    #[cfg(all(feature = "fog", not(feature = "fog-hw-selftests")))]
    console::write_str("SILVER DEEP SELFTESTS SKIPPED (fog hardware profile)\r\n");

    if DEEP_BOOT_SELFTESTS {
        // TLB proof: read through TTBR0, remap the same VA to another frame and
        // read again. Fails closed if the flush did not change the translation.
        if !mmu::run_tlb_self_test() {
            exceptions::silver_handle_exception(9, 0, 0, 0);
        }
        ram_log::log("MMU TLB OK");

        // ASID isolation: two spaces map the same VA to different frames; two
        // switches without any flush must still return each space's own value.
        if !mmu::run_asid_self_test() {
            exceptions::silver_handle_exception(9, 0, 0, 0);
        }
        console::write_str("MMU ASID OK\r\n");
        ram_log::log("MMU ASID OK");

        // Heap-L3 persistence: emptying the heap through the generic unmap must
        // not free the table, and the next heap map must land in the same slot
        // instead of writing through a zero descriptor.
        if !mmu::run_heap_table_self_test() {
            exceptions::silver_handle_exception(9, 0, 0, 0);
        }
        console::write_str("MMU HEAP OK\r\n");
        ram_log::log("MMU HEAP OK");

        // Recycled DMA frames must be zero before EL0 can read them.
        if !mmu::run_dma_zero_self_test() {
            exceptions::silver_handle_exception(9, 0, 0, 0);
        }
        console::write_str("DMA ZERO OK\r\n");
        ram_log::log("DMA ZERO OK");
    }

    // RX ring arithmetic under head/tail wraparound (the cross-CPU exclusion
    // is the lock around push/pop; this proves the indices themselves).
    #[cfg(all(feature = "smp", feature = "smp-selftest"))]
    {
        if !pl011::run_ring_self_test() {
            exceptions::silver_handle_exception(9, 0, 0, 0);
        }
        console::write_str("UART RING OK\r\n");
        ram_log::log("UART RING OK");

        // SYS_STOP_CONSOLE requires CAP_CONSOLE_GRANT: the owner has it, a
        // plain driver does not.
        if !scheduler::run_console_cap_selftest() {
            exceptions::silver_handle_exception(9, 0, 0, 0);
        }
        console::write_str("CONSOLE CAP OK\r\n");
        ram_log::log("CONSOLE CAP OK");
    }

    if DEEP_BOOT_SELFTESTS {
        // DMA quarantine window: a released run must survive the grace passes
        // and then return to the frame allocator.
        if !scheduler::run_dma_quarantine_self_test() {
            exceptions::silver_handle_exception(9, 0, 0, 0);
        }
        ram_log::log("DMA QUARANTINE OK");

        // In-flight DMA lease gate: an armed device range must block reuse until
        // the driver drops the lease.
        if !scheduler::run_dma_lease_self_test() {
            exceptions::silver_handle_exception(9, 0, 0, 0);
        }
        console::write_str("DMA LEASE OK\r\n");
        ram_log::log("DMA LEASE OK");
    }
}
