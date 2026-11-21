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
        // Fault forensics: the mmap region around FAR and the dying task's
        // last syscalls. Both are read-only ring/region walks, no allocation.
        scheduler::debug_fault_region(far as usize);
        scheduler::debug_task_trace(task_id, 10);
        scheduler::debug_lazy_fail(task_id, 6);
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
pub const SYNTH_RING_LEN: usize = 32;
pub static SYNTH_RING_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
#[no_mangle]
pub static mut SYNTH_RING: [[u64; 6]; SYNTH_RING_LEN] = [[0; 6]; SYNTH_RING_LEN];

pub const DUMP_RING_LEN: usize = 4;
pub const DUMP_LINE_MAX: usize = 2048;
pub static DUMP_RING_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
pub static mut DUMP_RING_LENGTHS: [u32; DUMP_RING_LEN] = [0; DUMP_RING_LEN];
#[no_mangle]
pub static mut DUMP_RING: [u8; DUMP_RING_LEN * DUMP_LINE_MAX] =
    [0; DUMP_RING_LEN * DUMP_LINE_MAX];

pub const FAULT_RING_LEN: usize = 32;
pub static FAULT_RING_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
#[no_mangle]
pub static mut FAULT_RING: [[u64; 6]; FAULT_RING_LEN] =
    [[0, 0, 0, 0, 0, 0]; FAULT_RING_LEN];

fn fault_ring_record(esr: u64, elr: u64, far: u64) {
    let index = FAULT_RING_INDEX.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let task = scheduler::current_task_id();
    // Region coverage of the faulting page in the thread-group owner's mmap
    // list (0 none, 1 non-lazy, 2 lazy, 3 lazy with zero prot), recorded at
    // fault entry so the ring shows context even after later faults wrap it.
    let coverage = scheduler::fault_coverage(far as usize);
    unsafe {
        let ring = core::ptr::addr_of_mut!(FAULT_RING);
        (*ring)[index % FAULT_RING_LEN] = [task, esr, elr, far, coverage.0, coverage.1];
    }
}

#[cfg(feature = "linux-abi")]
#[no_mangle]
pub extern "C" fn silver_handle_user_fault(
    _frame: *mut u64,
    esr: u64,
    elr: u64,
    far: u64,
) -> u64 {
    fault_ring_record(esr, elr, far);
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
        let result = crate::scheduler::cow_fault(far as usize, esr);
        // esr=0x40 marks a kernel-initiated copy (back_user_pages): the
        // syscall wrote into a user buffer that is not backed. Record the
        // calling syscall and its buffer pointer - that identifies which
        // ioctl/read handed the kernel a bad address.
        if !result && esr == 0x40 {
            let index = SYNTH_RING_INDEX.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            unsafe {
                let frame = _frame;
                let x8 = *frame.add(8);
                let x0 = *frame.add(0);
                let x1 = *frame.add(1);
                let ring = core::ptr::addr_of_mut!(SYNTH_RING);
                (*ring)[index % SYNTH_RING_LEN] =
                    [scheduler::current_task_id(), x8, x0, x1, far, 0];
            }
        }
        result
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
    let mut line = [0_u8; 4096];
    let mut length = 0_usize;

    push_bytes(&mut line, &mut length, b"FAULT task=");
    push_number(&mut line, &mut length, crate::scheduler::current_task_id());
    push_bytes(&mut line, &mut length, b" esr=0x");
    push_hex(&mut line, &mut length, esr);
    push_bytes(&mut line, &mut length, b" far=0x");
    push_hex(&mut line, &mut length, far);
    push_bytes(&mut line, &mut length, b" elr=0x");
    push_hex(&mut line, &mut length, elr);
    // Identify the program: which binary faulted (busybox applet, openrc,
    // phoc, ...) decides the whole investigation path.
    push_bytes(&mut line, &mut length, b" argv=");
    let mut argv = [0_u8; 96];
    let argv_length = crate::scheduler::current_task_argv_prefix(&mut argv);
    push_bytes(&mut line, &mut length, &argv[..argv_length]);
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
    {
        let mut diag = [0_u8; 160];
        let diag_length = crate::scheduler::cow_diagnostic(far as usize, &mut diag);
        push_bytes(&mut line, &mut length, &diag[..diag_length]);
        push_bytes(&mut line, &mut length, b"\r\n");
    }
    // Hex-dump the memory around x19 (frame[19]): for corrupted-object faults
    // the shape of the neighborhood (pointer-like? protocol bytes?) says
    // whether a wild kernel copy or an object overwrite is the culprit. The
    // read is fault-safe and skips silently when x19 is not user memory.
    {
        const DUMP_RANGE: i64 = 96;
        let x19 = unsafe { *frame.add(19) };
        let start = x19.saturating_sub(DUMP_RANGE as u64) & !0x7;
        let mut dump = [0_u8; 2 * DUMP_RANGE as usize];
        // Only probe plausibly-user pointers: a kernel VA here (a wild
        // register at fault time) must not drive any page-table walk.
        if start < (1_u64 << 48)
            && crate::user_memory::copy_from_user(start, &mut dump).is_ok()
        {
            push_bytes(&mut line, &mut length, b"MEM x19-96:\r\n");
            for word in dump.chunks_exact(8) {
                push_bytes(&mut line, &mut length, b"  0x");
                push_hex(&mut line, &mut length, start + (word.as_ptr() as usize - dump.as_ptr() as usize) as u64);
                push_bytes(&mut line, &mut length, b"=0x");
                push_hex(
                    &mut line,
                    &mut length,
                    u64::from_le_bytes(word.try_into().unwrap()),
                );
                push_bytes(&mut line, &mut length, b"\r\n");
            }
        }
    }
    // The instruction bytes at ELR-16, read from the faulting task's own
    // address space: they identify the code that actually faulted without
    // trusting any symbol/base assumptions.
    {
        let mut code = [0_u8; 16];
        let base = elr.saturating_sub(16) & !0x3;
        if crate::user_memory::copy_from_user(base, &mut code).is_ok() {
            push_bytes(&mut line, &mut length, b"CODE elr-16=0x");
            push_hex(&mut line, &mut length, base);
            push_bytes(&mut line, &mut length, b":");
            for word in code.chunks_exact(4) {
                let insn = u32::from_le_bytes(word.try_into().unwrap());
                push_bytes(&mut line, &mut length, b" ");
                let mut digits = [0_u8; 8];
                for (index, byte) in digits.iter_mut().enumerate() {
                    let nibble = ((insn >> (28 - 4 * index)) & 0xF) as u8;
                    *byte = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
                }
                push_bytes(&mut line, &mut length, &digits);
            }
            push_bytes(&mut line, &mut length, b"\r\n");
        }
    }
    // Physical backing of the faulting page and of x19's page: if far's
    // frame is owned by another region somewhere, the alias report and this
    // line together nail the corruption down.
    let ttbr0 = crate::scheduler::current_task_ttbr0();
    for (label, address) in [("far", far), ("x19", unsafe { *frame.add(19) })] {
        if let Some(physical) =
            crate::mmu::user_physical_address(ttbr0, address as usize & !0xFFF)
        {
            push_bytes(&mut line, &mut length, b"PHYS ");
            push_bytes(&mut line, &mut length, label.as_bytes());
            push_bytes(&mut line, &mut length, b" page=0x");
            push_hex(&mut line, &mut length, physical as u64 & !0xFFF);
            push_bytes(&mut line, &mut length, b"\r\n");
        }
    }
    console::write_bytes(&line[..length]);
    // The full report also lands in a static ring: console writes drop under
    // load, and the QEMU monitor pmemsave path reads this reliably. Four
    // slots, each holding one complete dump line.
    DUMP_RING_INDEX.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    {
        let slot = (DUMP_RING_INDEX.load(core::sync::atomic::Ordering::Relaxed) - 1) % DUMP_RING_LEN;
        let take = length.min(DUMP_LINE_MAX);
        unsafe {
            let base = core::ptr::addr_of_mut!(DUMP_RING) as *mut u8;
            let destination = base.add(slot * DUMP_LINE_MAX);
            core::ptr::copy_nonoverlapping(line.as_ptr(), destination, take);
            let lengths = core::ptr::addr_of_mut!(DUMP_RING_LENGTHS) as *mut u32;
            *lengths.add(slot) = take as u32;
        }
    }
    // The virtual map decides whether the corruption is a VA collision: two
    // lines sharing an address range are the smoking gun.
    crate::scheduler::dump_current_task_map();
    crate::scheduler::dump_physical_alias_report();
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
