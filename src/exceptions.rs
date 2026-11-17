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
/// EL0 fault entry from the vector. Returns `0` when a write to a COW page was
/// repaired (the trampoline retries the instruction); any other fault falls
/// through to the normal task kill, which never returns.
#[cfg(feature = "linux-abi")]
#[no_mangle]
pub extern "C" fn silver_handle_user_fault(
    _frame: *mut u64,
    esr: u64,
    elr: u64,
    far: u64,
) -> u64 {
    #[cfg(feature = "gdb-stub")]
    if crate::debug::is_watchpoint(esr) {
        crate::debug::disarm();
        dump_user_frame(_frame, esr, elr, far);
        crate::gdb::serve(_frame, esr, elr, far);
        silver_handle_task_fault(esr, elr, far);
    }
    #[cfg(feature = "gdb-stub")]
    crate::debug::set_fault_context(far, elr);
    let recovered = {
        let _kernel_lock = crate::smp::BklGuard::new();
        crate::scheduler::cow_fault(far as usize, esr)
    };
    #[cfg(feature = "gdb-stub")]
    crate::debug::clear_fault_context();
    #[cfg(feature = "gdb-stub")]
    if crate::debug::take_soft_hit() {
        crate::debug::disarm();
        dump_user_frame(_frame, esr, elr, far);
        crate::gdb::serve(_frame, esr, elr, far);
        silver_handle_task_fault(esr, elr, far);
    }
    if recovered {
        0
    } else {
        dump_user_frame(_frame, esr, elr, far);
        #[cfg(feature = "gdb-stub")]
        crate::gdb::serve(_frame, esr, elr, far);
        silver_handle_task_fault(esr, elr, far)
    }
}

/// Without the Linux ABI there are no COW pages: any EL0 fault is final, but
/// the vector still references this symbol.
#[cfg(not(feature = "linux-abi"))]
#[no_mangle]
pub extern "C" fn silver_handle_user_fault(
    _frame: *mut u64,
    esr: u64,
    elr: u64,
    far: u64,
) -> u64 {
    silver_handle_task_fault(esr, elr, far)
}

/// Appends bytes to the fixed fault-report buffer.
fn push_bytes(line: &mut [u8], length: &mut usize, bytes: &[u8]) {
    let room = line.len().saturating_sub(*length);
    let take = bytes.len().min(room);
    line[*length..*length + take].copy_from_slice(&bytes[..take]);
    *length += take;
}

/// Appends a zero-padded 64-bit hex value (no `0x` prefix).
fn push_hex(line: &mut [u8], length: &mut usize, value: u64) {
    let mut digits = [b'0'; 16];
    for (digit, shift) in digits.iter_mut().zip((0..16).rev()) {
        let nibble = ((value >> (shift * 4)) & 0xF) as u8;
        *digit = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
    }
    push_bytes(line, length, &digits);
}

/// Appends an unsigned decimal value.
fn push_number(line: &mut [u8], length: &mut usize, mut value: u64) {
    let mut digits = [0_u8; 20];
    let mut count = 0_usize;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let mut reversed = [0_u8; 20];
    for index in 0..count {
        reversed[index] = digits[count - 1 - index];
    }
    push_bytes(line, length, &reversed[..count]);
}

/// Prints the saved EL0 register bank of the dying task.
///
/// The whole report is formatted into one buffer and published with a single
/// console write: two CPUs can fault at the same time, and a sequence of
/// small writes lets their registers interleave into a dump that then points
/// at the wrong task. The header repeats the task id, FAR, ELR and ESR so a
/// report can be attributed without relying on the adjacent "Task N killed"
/// line, which is a separate write.
fn dump_user_frame(frame: *mut u64, esr: u64, elr: u64, far: u64) {
    if frame.is_null() {
        return;
    }
    let names = [
        "x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10",
        "x11", "x12", "x13", "x14", "x15", "x16", "x17", "x18", "x19", "x20",
        "x21", "x22", "x23", "x24", "x25", "x26", "x27", "x28", "x29", "x30",
    ];
    let mut line = [0_u8; 1152];
    let mut length = 0_usize;

    push_bytes(&mut line, &mut length, b"FAULT task=");
    push_number(&mut line, &mut length, crate::scheduler::current_task_id());
    push_bytes(&mut line, &mut length, b" esr=0x");
    push_hex(&mut line, &mut length, esr);
    push_bytes(&mut line, &mut length, b" far=0x");
    push_hex(&mut line, &mut length, far);
    push_bytes(&mut line, &mut length, b" elr=0x");
    push_hex(&mut line, &mut length, elr);
    push_bytes(&mut line, &mut length, b"\r\n");
    for (index, name) in names.iter().enumerate() {
        push_bytes(&mut line, &mut length, name.as_bytes());
        push_bytes(&mut line, &mut length, b"=0x");
        push_hex(&mut line, &mut length, unsafe { *frame.add(index) });
        push_bytes(
            &mut line,
            &mut length,
            if index % 4 == 3 { b"\r\n" } else { b" " },
        );
    }
    let sp: u64;
    unsafe {
        core::arch::asm!("mrs {}, sp_el0", out(reg) sp);
    }
    push_bytes(&mut line, &mut length, b"SP_EL0=0x");
    push_hex(&mut line, &mut length, sp);
    push_bytes(&mut line, &mut length, b"\r\n");
    console::write_bytes(&line[..length]);
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
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
