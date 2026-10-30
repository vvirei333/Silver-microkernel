use crate::console;
use crate::scheduler;

#[no_mangle]
pub extern "C" fn silver_handle_task_fault(esr: u64, elr: u64, far: u64) -> ! {
    // Reached from the EL0 sync vector with IRQs already masked and x0..x2
    // carrying ESR_EL1/ELR_EL1/FAR_EL1. console writes are raw MMIO pokes whose
    // TX wait restores the caller's DAIF, so they are safe in exception
    // context; nothing here may allocate or take a lock (the dying task may
    // still hold one). Format matches the other kernel dumps: fixed-width
    // uppercase hex, \r\n line ends.
    #[cfg(not(feature = "tablet"))]
    {
        let task_id = scheduler::current_task_id();
        console::write_str("Task ");
        console::write_dec_u64(task_id);
        console::write_str(" killed: Page Fault at 0x");
        console::write_hex_u64(far);
        console::write_str(" (ESR: 0x");
        console::write_hex_u64(esr);
        console::write_str(")\r\n");
        console::write_str("ELR_EL1=");
        console::write_hex_u64(elr);
        console::write_str("\r\n");
        scheduler::dump_switch_trace();
    }
    #[cfg(feature = "tablet")]
    {
        crate::ram_log::log("TASK FAULT FAR");
        crate::ram_log::log_hex(far);
        crate::ram_log::log_raw(" ESR ");
        crate::ram_log::log_hex(esr);
        crate::ram_log::log_raw(" ELR ");
        crate::ram_log::log_hex(elr);
        crate::ram_log::log_raw("\n");
    }
    // The task touched something it must not (unmapped VA, privileged MSR,
    // EL1-only MMIO...). The kernel survives; the guilty task does not.
    // exit_current marks it Dead, hands back the console and schedules away;
    // reap_dead_tasks then releases its stack, TTBR0 tables and ELF image.
    scheduler::exit_current();
}
#[no_mangle]
pub extern "C" fn silver_handle_exception(kind: u64, esr: u64, elr: u64, far: u64) -> ! {
    // EL1 / kernel path: an exception taken from EL1 (or a boot/allocator/IRQ
    // failure reported through kind 4..8) is a kernel bug that cannot be
    // survived — resuming risks corrupting kernel state. Dump the registers,
    // then halt in a wfe loop (the same idiom the boot stub uses), instead of
    // exiting the current task and pretending nothing happened.
    // On the tablet there must be NO console write here: an abort while the
    // MMU/vector state is unknown goes straight to EL3 (QCDUMP) and any UART
    // poke only adds a second abort before the log is written.
    #[cfg(not(feature = "tablet"))]
    {
        console::write_str("EL1 EXCEPTION kind=");
        console::write_dec_u64(kind);
        console::write_str("\r\n");
        console::write_str("ESR_EL1=");
        console::write_hex_u64(esr);
        console::write_str("\r\n");
        console::write_str("ELR_EL1=");
        console::write_hex_u64(elr);
        console::write_str("\r\n");
        console::write_str("FAR_EL1=");
        console::write_hex_u64(far);
        console::write_str("\r\n");
        console::write_str("KERNEL PANIC: halting\r\n");
    }
    // Leave the exception in the pstore ramoops zone so the next Android boot
    // can read it back (the UART may be invisible on real hardware).
    crate::ram_log::log_raw("EXC kind=");
    crate::ram_log::log_hex(kind);
    crate::ram_log::log_raw(" ESR=");
    crate::ram_log::log_hex(esr);
    crate::ram_log::log_raw(" ELR=");
    crate::ram_log::log_hex(elr);
    crate::ram_log::log_raw(" FAR=");
    crate::ram_log::log_hex(far);
    crate::ram_log::log_raw("\n");
    crate::ram_log::fb_fill(255, 0, 0); // RED SCREEN = kernel exception
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
