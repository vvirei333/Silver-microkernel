use crate::pl011;
use crate::scheduler;

#[no_mangle]
pub extern "C" fn silver_handle_task_fault(esr: u64, elr: u64, far: u64) -> ! {
    // Reached from the EL0 sync vector with IRQs already masked and x0..x2
    // carrying ESR_EL1/ELR_EL1/FAR_EL1. pl011 writes are raw MMIO pokes whose
    // TX wait restores the caller's DAIF, so they are safe in exception
    // context; nothing here may allocate or take a lock (the dying task may
    // still hold one). Format matches the other kernel dumps: fixed-width
    // uppercase hex, \r\n line ends.
    let task_id = scheduler::current_task_id();
    pl011::write_str("Task ");
    pl011::write_dec_u64(task_id);
    pl011::write_str(" killed: Page Fault at 0x");
    pl011::write_hex_u64(far);
    pl011::write_str(" (ESR: 0x");
    pl011::write_hex_u64(esr);
    pl011::write_str(")\r\n");
    pl011::write_str("ELR_EL1=");
    pl011::write_hex_u64(elr);
    pl011::write_str("\r\n");
    scheduler::dump_switch_trace();
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
    pl011::write_str("EL1 EXCEPTION kind=");
    pl011::write_dec_u64(kind);
    pl011::write_str("\r\n");
    pl011::write_str("ESR_EL1=");
    pl011::write_hex_u64(esr);
    pl011::write_str("\r\n");
    pl011::write_str("ELR_EL1=");
    pl011::write_hex_u64(elr);
    pl011::write_str("\r\n");
    pl011::write_str("FAR_EL1=");
    pl011::write_hex_u64(far);
    pl011::write_str("\r\n");
    pl011::write_str("KERNEL PANIC: halting\r\n");
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
