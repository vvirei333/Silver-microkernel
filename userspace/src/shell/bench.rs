//! Benchmarks and the memory smoke test.
//!
//! Timings are cycle-accurate: the shell asks the kernel for the counter and
//! its frequency once, then reports ns/op and throughput per section.

use crate::syscall;

pub(super) fn memtest() {
    const SIZE: usize = 1024 * 1024;
    let Some(base) = syscall::mmap(0, SIZE, syscall::PROT_READ | syscall::PROT_WRITE) else {
        syscall::print("memtest: mmap failed\r\n");
        return;
    };
    syscall::print("memtest: mapped 1 MiB at 0x");
    syscall::print_hex_u64(base as u64);
    syscall::print("\r\n");

    let pointer = base as *mut u8;
    let message = b"Silver memtest!";
    for (index, &byte) in message.iter().enumerate() {
        unsafe { core::ptr::write_volatile(pointer.add(index), byte) };
    }

    let mut round_trip = true;
    for (index, &byte) in message.iter().enumerate() {
        if unsafe { core::ptr::read_volatile(pointer.add(index)) } != byte {
            round_trip = false;
        }
    }
    syscall::print(if round_trip {
        "memtest: string round-trip OK (\"Silver memtest!\")\r\n"
    } else {
        "memtest: string MISMATCH\r\n"
    });

    for index in 0..SIZE {
        unsafe { core::ptr::write_volatile(pointer.add(index), (index & 0xFF) as u8) };
    }
    let mut pattern_ok = true;
    for index in 0..SIZE {
        if unsafe { core::ptr::read_volatile(pointer.add(index)) } != (index & 0xFF) as u8 {
            pattern_ok = false;
            break;
        }
    }
    syscall::print(if pattern_ok {
        "memtest: 1 MiB pattern OK\r\n"
    } else {
        "memtest: 1 MiB pattern MISMATCH\r\n"
    });

    if syscall::munmap(base, SIZE) {
        syscall::print("memtest: unmapped\r\n");
    } else {
        syscall::print("memtest: munmap failed\r\n");
    }
}

// ---------------------------------------------------------------------------
// bench — cycle-accurate fast-path measurements
// ---------------------------------------------------------------------------

/// Iterations per micro-benchmark: long enough to average out the counter's
/// one-tick granularity, short enough that `bench` stays a shell command and
/// not a test suite. Every section runs in well under a second on QEMU.
const BENCH_SYSCALL_ITERATIONS: u64 = 100_000;

const BENCH_YIELD_ITERATIONS: u64 = 5_000;

const BENCH_FS_ITERATIONS: u64 = 2_000;

const BENCH_VM_ITERATIONS: u64 = 32;

const BENCH_MEMCPY_ITERATIONS: u64 = 400;

const BENCH_VM_SIZE: usize = 1024 * 1024;

const BENCH_MEMCPY_SIZE: usize = 64 * 1024;

const BENCH_READ_SIZE: usize = 4096;

const BENCH_PAGE_SIZE: usize = 4096;

/// `bench [syscall|sched|fs|vm|memcpy|all]`
///
/// Timings are read from `CNTPCT_EL0` at EL0 (the kernel unmasks it per CPU in
/// `timer::enable_el0_counter`), so the measured window contains no clock
/// syscall of its own. Numbers are per-operation costs of the paths user space
/// actually exercises: a trap round trip, a scheduler yield, the SFS read path
/// through the block cache, an anonymous page fault, and a raw copy baseline
/// that separates memory bandwidth from filesystem work.
pub(super) fn bench_command(section: Option<&str>) {
    let sections = ["syscall", "sched", "fs", "vm", "memcpy"];
    let all = match section {
        None | Some("all") => true,
        Some(name) => {
            if !sections.contains(&name) {
                syscall::print("Usage: bench [syscall|sched|fs|vm|memcpy|all]\r\n");
                return;
            }
            false
        }
    };

    let frequency = syscall::counter_frequency();
    syscall::print("WerewolfOS bench — counter ");
    syscall::print_dec_u64(frequency / 1_000_000);
    syscall::print(".");
    syscall::print_dec_u64((frequency % 1_000_000) / 100_000);
    syscall::print(" MHz, EL0 CNTPCT\r\n");

    if all || section == Some("syscall") {
        bench_syscall(frequency);
    }
    if all || section == Some("sched") {
        bench_yield(frequency);
    }
    if all || section == Some("fs") {
        bench_fs(frequency);
    }
    if all || section == Some("vm") {
        bench_vm(frequency);
    }
    if all || section == Some("memcpy") {
        bench_memcpy(frequency);
    }
}

/// One `ops  ns/op  ops/s` row, left-padding the label to a fixed column so a
/// full run lines up without a formatter (user space is `no_std`).
pub(super) fn bench_row(label: &str, operations: u64, cycles: u64, frequency: u64) {
    syscall::print("  ");
    syscall::print(label);
    let mut width = label.len();
    while width < 24 {
        syscall::print(" ");
        width += 1;
    }
    syscall::print_dec_u64(operations);
    syscall::print(" ops  ");
    syscall::print_dec_u64(bench_ns_per_op(operations, cycles, frequency));
    syscall::print(" ns/op  ");
    syscall::print_dec_u64(bench_ops_per_second(operations, cycles, frequency));
    syscall::print(" ops/s\r\n");
}

/// Nanoseconds per operation. The multiply is only taken when it fits in 64
/// bits (a multi-second run does not), otherwise seconds are scaled first.
pub(super) fn bench_ns_per_op(operations: u64, cycles: u64, frequency: u64) -> u64 {
    if operations == 0 || frequency == 0 {
        return 0;
    }
    let denominator = frequency.saturating_mul(operations).max(1);
    if cycles <= u64::MAX / 1_000_000_000 {
        cycles.saturating_mul(1_000_000_000) / denominator
    } else {
        (cycles / frequency.max(1)).saturating_mul(1_000_000_000) / operations.max(1)
    }
}

pub(super) fn bench_ops_per_second(operations: u64, cycles: u64, frequency: u64) -> u64 {
    if cycles == 0 || frequency == 0 {
        return 0;
    }
    // operations * frequency overflows before it is useful for the same reason
    // `bench_ns_per_op` guards its product; scale the numerator down first.
    if operations <= u64::MAX / frequency.max(1) {
        operations.saturating_mul(frequency) / cycles
    } else {
        (operations / cycles.max(1)).saturating_mul(frequency)
    }
}

/// MiB/s for a copy of `bytes` performed in `cycles`.
pub(super) fn bench_mib_per_second(bytes: u64, cycles: u64, frequency: u64) -> u64 {
    if cycles == 0 || frequency == 0 {
        return 0;
    }
    const MIB: u64 = 1024 * 1024;
    if bytes <= u64::MAX / frequency.max(1) {
        bytes.saturating_mul(frequency) / cycles / MIB
    } else {
        (bytes / cycles.max(1)).saturating_mul(frequency) / MIB
    }
}

pub(super) fn bench_syscall(frequency: u64) {
    let mut sink = 0_u64;
    let start = syscall::counter();
    for _ in 0..BENCH_SYSCALL_ITERATIONS {
        sink = sink.wrapping_add(syscall::task_id());
    }
    let cycles = syscall::counter().wrapping_sub(start);
    core::hint::black_box(sink);
    bench_row("syscall (SYS_WHOAMI)", BENCH_SYSCALL_ITERATIONS, cycles, frequency);
}

pub(super) fn bench_yield(frequency: u64) {
    let start = syscall::counter();
    for _ in 0..BENCH_YIELD_ITERATIONS {
        syscall::sleep(0);
    }
    let cycles = syscall::counter().wrapping_sub(start);
    bench_row("sched yield (sleep 0)", BENCH_YIELD_ITERATIONS, cycles, frequency);
}

pub(super) fn bench_fs(frequency: u64) {
    // Open/close alone first: it separates path resolution + inode lookup from
    // the data path, which is what tells us whether a slow `fs` row is the
    // block cache or a directory walk.
    let mut opened = 0_u64;
    let start = syscall::counter();
    for _ in 0..BENCH_FS_ITERATIONS {
        let Some(fd) = syscall::fs_open("/sfs/readme.txt", 0) else {
            break;
        };
        let _ = syscall::fs_close(fd);
        opened += 1;
    }
    let open_cycles = syscall::counter().wrapping_sub(start);
    if opened > 0 {
        bench_row("fs open+close (sfs)", opened, open_cycles, frequency);
    }

    // Same VFS path over the in-memory backend. Whatever this row costs is
    // VFS + syscall overhead; the difference to the row above is the on-disk
    // reader (path walk through directory blocks plus the block cache).
    let ram_path = "/tmp/bench.tmp";
    let _ = syscall::fs_create(ram_path);
    let mut ram_opened = 0_u64;
    let start = syscall::counter();
    for _ in 0..BENCH_FS_ITERATIONS {
        let Some(fd) = syscall::fs_open(ram_path, 0) else {
            break;
        };
        let _ = syscall::fs_close(fd);
        ram_opened += 1;
    }
    let ram_cycles = syscall::counter().wrapping_sub(start);
    if ram_opened > 0 {
        bench_row("fs open+close (ram)", ram_opened, ram_cycles, frequency);
    }

    let mut buffer = [0_u8; BENCH_READ_SIZE];
    let mut bytes = 0_u64;
    let mut completed = 0_u64;
    let start = syscall::counter();
    for _ in 0..BENCH_FS_ITERATIONS {
        let Some(fd) = syscall::fs_open("/sfs/readme.txt", 0) else {
            break;
        };
        bytes = bytes.wrapping_add(syscall::fs_read(fd, &mut buffer).unwrap_or(0) as u64);
        let _ = syscall::fs_close(fd);
        completed += 1;
    }
    let cycles = syscall::counter().wrapping_sub(start);
    if completed == 0 {
        syscall::print("  fs (open+read+close)     skipped: /sfs/readme.txt is not readable\r\n");
        return;
    }
    bench_row("fs open+read+close (sfs)", completed, cycles, frequency);
    syscall::print("    read ");
    syscall::print_dec_u64(bytes);
    syscall::print(" bytes at ");
    syscall::print_dec_u64(bench_mib_per_second(bytes, cycles, frequency));
    syscall::print(" MiB/s\r\n");
}

pub(super) fn bench_vm(frequency: u64) {
    let Some(base) = syscall::mmap(0, BENCH_VM_SIZE, syscall::PROT_READ | syscall::PROT_WRITE)
    else {
        syscall::print("  vm (first/warm touch)    skipped: mmap failed\r\n");
        return;
    };
    let pages = BENCH_VM_SIZE / BENCH_PAGE_SIZE;

    // First sweep: every page is backed by a fresh frame, so this is the fault
    // path plus the zero-page handoff.
    let start = syscall::counter();
    touch_pages(base, pages);
    let first_cycles = syscall::counter().wrapping_sub(start);
    bench_row("vm first touch (1 MiB)", pages as u64, first_cycles, frequency);

    // Remaining sweeps: same pages, now resident. The difference against the
    // first sweep is the cost of faulting rather than of the accesses.
    let start = syscall::counter();
    for _ in 1..BENCH_VM_ITERATIONS {
        touch_pages(base, pages);
    }
    let warm_cycles = syscall::counter().wrapping_sub(start);
    bench_row("vm warm touch (1 MiB)", pages as u64 * (BENCH_VM_ITERATIONS - 1), warm_cycles, frequency);

    if !syscall::munmap(base, BENCH_VM_SIZE) {
        syscall::print("  vm: munmap failed\r\n");
    }
}

/// Writes then reads the first byte of every page so the compiler cannot elide
/// the accesses and so each page is genuinely mapped in.
pub(super) fn touch_pages(base: usize, pages: usize) {
    for page in 0..pages {
        let address = (base + page * BENCH_PAGE_SIZE) as *mut u8;
        unsafe {
            core::ptr::write_volatile(address, (page & 0xFF) as u8);
            core::hint::black_box(core::ptr::read_volatile(address));
        }
    }
}

pub(super) fn bench_memcpy(frequency: u64) {
    let Some(base) = syscall::mmap(0, BENCH_MEMCPY_SIZE * 2, syscall::PROT_READ | syscall::PROT_WRITE)
    else {
        syscall::print("  memcpy 64 KiB            skipped: mmap failed\r\n");
        return;
    };
    let source = base as *const u8;
    let destination = (base + BENCH_MEMCPY_SIZE) as *mut u8;
    // Warm both halves before timing so the measured loop is pure copy.
    touch_pages(base, BENCH_MEMCPY_SIZE * 2 / BENCH_PAGE_SIZE);

    let start = syscall::counter();
    for _ in 0..BENCH_MEMCPY_ITERATIONS {
        unsafe { core::ptr::copy_nonoverlapping(source, destination, BENCH_MEMCPY_SIZE) };
        core::hint::black_box(unsafe { core::ptr::read_volatile(destination) });
    }
    let cycles = syscall::counter().wrapping_sub(start);
    let bytes = (BENCH_MEMCPY_SIZE as u64).saturating_mul(BENCH_MEMCPY_ITERATIONS);
    bench_row("memcpy 64 KiB", BENCH_MEMCPY_ITERATIONS, cycles, frequency);
    syscall::print("    ");
    syscall::print_dec_u64(bench_mib_per_second(bytes, cycles, frequency));
    syscall::print(" MiB/s\r\n");

    if !syscall::munmap(base, BENCH_MEMCPY_SIZE * 2) {
        syscall::print("  memcpy: munmap failed\r\n");
    }
}