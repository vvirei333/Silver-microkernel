#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(feature = "std"), no_main)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
mod allocator;
#[cfg(not(feature = "std"))]
mod boot;
#[cfg(not(feature = "std"))]
mod boot_metrics;
#[cfg(not(feature = "std"))]
mod cache;
#[cfg(not(feature = "std"))]
mod capability;
#[cfg(not(feature = "std"))]
mod channel;
#[cfg(not(feature = "std"))]
mod condvar;
#[cfg(not(feature = "std"))]
mod console;
#[cfg(not(feature = "std"))]
mod elf;
#[cfg(not(feature = "std"))]
mod entropy;
#[cfg(feature = "linux-abi")]
mod linux_elf;
/// Without the `linux-abi` feature the Linux image type still exists as an
/// uninhabited placeholder so the task plumbing can carry `Option<LinuxImage>`
/// (always `None`) — but no Linux loader, syscall or ELF code is compiled.
#[cfg(not(feature = "linux-abi"))]
mod linux_elf {
    #[allow(dead_code)]
    #[derive(Copy, Clone)]
    pub struct LinuxImage;
}
#[cfg(not(feature = "std"))]
mod elf_loader;
#[cfg(not(feature = "std"))]
mod exceptions;
#[cfg(not(feature = "std"))]
// `fdt` and `devices` are shared with the library instead of being compiled a
// second time here: one implementation, one set of host tests, and the bin only
// adds the hardware half. Both stay reachable as `crate::fdt` / `crate::devices`.
pub use silver_microkernel::devices;
pub use silver_microkernel::fdt;
#[cfg(not(feature = "std"))]
mod fog;
#[cfg(not(feature = "std"))]
mod fog_validate;
#[cfg(not(feature = "std"))]
mod frame_allocator;
#[cfg(not(feature = "std"))]
mod geni_uart;
mod driver_resources;
#[cfg(not(feature = "std"))]
mod gic;
mod gicv3;
mod gicv3_policy;
mod irq_policy;
#[cfg(not(feature = "std"))]
mod interrupts;
#[cfg(not(feature = "std"))]
mod ipc;
#[cfg(all(not(feature = "std"), feature = "linux-abi"))]
mod ivshmem;
#[cfg(not(feature = "std"))]
mod kernel_stack;
#[cfg(not(feature = "std"))]
mod memory;
#[cfg(not(feature = "std"))]
mod mmu;
#[cfg(not(feature = "std"))]
mod mutex;
#[cfg(not(feature = "std"))]
mod pl011;
#[cfg(feature = "gdb-stub")]
mod gdb;
#[cfg(feature = "gdb-stub")]
mod debug;
#[cfg(not(feature = "std"))]
mod platform;
mod power;
mod ram_log;
// The boot stub mirrors this policy in assembly; the Rust copy exists for
// host boundary tests, so the binary itself does not call these items.
#[allow(dead_code)]
mod reloc;
#[cfg(not(feature = "std"))]
mod scheduler;
#[cfg(not(feature = "std"))]
mod seed;
#[cfg(not(feature = "std"))]
mod semaphore;
mod smp;
#[cfg(not(feature = "std"))]
mod syscall;
#[cfg(feature = "linux-abi")]
mod linux_abi;
#[cfg(feature = "linux-abi")]
mod linux_fd;
#[cfg(feature = "linux-abi")]
mod linux_signal;
#[cfg(not(feature = "std"))]
mod timer;
#[cfg(not(feature = "std"))]
mod user_memory;
#[cfg(not(feature = "std"))]
mod sfs;
mod sfs_format;
mod user_program;
#[cfg(feature = "linux-abi")]
mod drm;
mod sysfs;
mod virtio_blk;
mod virtio_gpu;
#[cfg(all(not(feature = "std"), feature = "smp"))]
mod watchdog;
#[cfg(not(feature = "std"))]
mod vfs;
#[cfg(not(feature = "std"))]
mod wait_queue;
mod procfs;

#[cfg(not(feature = "std"))]
use core::panic::PanicInfo;

/// C-ABI entry point called from the pre-relocation boot stub. Kept in the
/// binary (not the shared library) so the host-testable module does not emit
/// a second global symbol when Cargo links both crates.
#[no_mangle]
#[inline(never)]
pub unsafe extern "C" fn fog_validate_relocation(
    dtb: *const u8,
    dst_start: u64,
    dst_end: u64,
    src_start: u64,
    src_end: u64,
) -> u64 {
    fog_validate::validate_relocation(dtb, dst_start, dst_end, src_start, src_end)
}

#[cfg(all(feature = "std", feature = "no_std"))]
compile_error!("Features 'std' and 'no_std' are mutually exclusive");

#[cfg(all(not(feature = "std"), not(target_arch = "aarch64")))]
compile_error!("The bare-metal kernel currently supports only AArch64");


/// The newer MMU/ASID/DMA boot proofs run on QEMU and on any non-fog build.
/// A fog hardware image skips them unless `fog-hw-selftests` is requested:
/// the TLB proof was observed to wedge a `rain` unit before userspace.
#[cfg(all(feature = "fog", not(feature = "fog-hw-selftests")))]
const DEEP_BOOT_SELFTESTS: bool = false;
#[cfg(any(not(feature = "fog"), feature = "fog-hw-selftests"))]
const DEEP_BOOT_SELFTESTS: bool = true;

/// Minimal boot framebuffer proof. On the phone the Redmi `cont_splash`
/// region is painted directly; on QEMU (and any non-fog profile) a small
/// frame-allocator surface validates the writer without a display.
fn simplefb_boot() {
    #[cfg(feature = "fog")]
    {
        let info = silver_microkernel::simplefb::FOG_PANEL;
        let Some(bytes) = info.bytes() else {
            return;
        };
        let pointer = memory::phys_to_virt(info.base as usize) as *mut u32;
        let pixels = unsafe { core::slice::from_raw_parts_mut(pointer, bytes / 4) };
        let Some(mut canvas) = silver_microkernel::simplefb::Canvas::new(
            pixels, info.width, info.height, info.stride,
        ) else {
            return;
        };
        canvas.paint_boot();
        console::write_str("SIMPLEFB: FOG base=0x");
        console::write_hex_u64(info.base);
        console::write_str(" 720x1650 stride=720\r\n");
        ram_log::log("SIMPLEFB FOG PANEL");
    }

    #[cfg(not(feature = "fog"))]
    {
        const WIDTH: usize = 128;
        const HEIGHT: usize = 96;
        const STRIDE: usize = 128;
        let pages = (WIDTH * HEIGHT * 4).div_ceil(memory::PAGE_SIZE);
        let Some((physical, generation)) =
            frame_allocator::alloc_contiguous_pages_tagged(pages)
        else {
            console::write_str("SIMPLEFB TEST: NO FRAMES\r\n");
            return;
        };
        let pointer = memory::phys_to_virt(physical) as *mut u32;
        let pixels = unsafe {
            core::slice::from_raw_parts_mut(pointer, STRIDE * HEIGHT)
        };
        let mut ok = false;
        let mut painted = false;
        if let Some(mut canvas) = silver_microkernel::simplefb::Canvas::new(
            pixels, WIDTH, HEIGHT, STRIDE,
        ) {
            canvas.paint_boot();
            painted = true;
        }
        if painted {
            ok = pixels[0] != 0
                && pixels[STRIDE * (HEIGHT / 3) + WIDTH / 2] == 0xFF39_D2FF;
        }
        frame_allocator::free_contiguous_pages_tagged(physical, pages, generation);
        if ok {
            console::write_str("SIMPLEFB TEST PASS 128x96\r\n");
        } else {
            console::write_str("SIMPLEFB TEST FAIL\r\n");
        }
    }
}

#[cfg(not(feature = "std"))]
#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    // Per-CPU identity first: every scheduler/lock path reads TPIDR_EL1.
    smp::init_cpu(0);

    // Fog: prove the machine from the live FDT before any device is touched.
    // `probe` only reads; it publishes the platform identity, the UART base
    // and one FDT-derived frame-allocator window. If the tree is missing or
    // not a khaje/SM6225 board, nothing is forced and the boot path fails
    // closed instead of inventing a map.
    #[cfg(feature = "fog")]
    let boot_params = boot::get().unwrap_or(boot::BootParams::zero());
    #[cfg(feature = "fog")]
    let fog_info = fog::probe(&boot_params);
    #[cfg(feature = "fog")]
    if fog_info.valid {
        memory::set_fog_available(
            fog_info.memory.start as usize,
            fog_info.memory.end as usize,
        );
    }

    // The tablet has no readable UART, so EVERY console/boot_metrics call is
    // compiled out there: a stray write to a QEMU address (PL011 0x09000000,
    // GICv2 0x08000000) would be a bus abort -> QCDUMP before any log. On the
    // tablet the only output channel is the ramoops RAM log.
    #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
    boot_metrics::mark_boot_start();

    // The ramoops console zone lives in reserved DRAM; on the tablet it is the
    // only log channel, on QEMU ram_log is a no-op (the UART is the channel).
    ram_log::init();
    ram_log::log("KERNEL ENTRY");

    mmu::init();

    // Devices before the console: on a new machine the tree is what names the
    // UART, so the console cannot be chosen before the tree is read. Parsing
    // needs the MMU (it reads through `phys_to_virt`), hence the order.
    // QEMU without a DTB keeps the fixed PL011/PCI table; fog gets the UART
    // base/size/INTID from its node. A missing/disabled node binds nothing.
    {
        let params = boot::get();
        let dtb = params.as_ref().and_then(fog::dtb_slice);
        crate::driver_resources::init(dtb);
        #[cfg(feature = "gdb-stub")]
        crate::gdb::probe_transport(dtb);
    }

    // Console first after that: a bad memory map must be *reported* before the
    // allocator asserts on an empty window.
    console::init();
    // Cache state is reported on every console-capable profile. The boot
    // self-test below re-checks the same bits on the APs, whose values are
    // recorded per CPU rather than inferred from this line.
    #[cfg(not(feature = "tablet"))]
    {
        console::write_str("CACHE=");
        if cache::caches_on(cache::read_sctlr()) {
            console::write_str("ON line=");
            console::write_dec_u64(cache::line_bytes() as u64);
        } else {
            console::write_str("OFF");
        }
        console::write_str("\r\n");
    }
    #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
    {
        console::write_str("PLATFORM=QEMU\r\n");
        console::write_str("MMU=OK\r\n");
        boot_metrics::log_milestone("MMU init");
    }

    // Size AP bring-up from the live tree instead of assuming MAX_CPUS. QEMU
    // passes a DTB even with `-kernel`; fog/tablet stay single-core by policy
    // and a missing tree leaves the old default in place.
    #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
    {
        let params = boot::get();
        let dtb = params.as_ref().and_then(fog::dtb_slice);
        let mut parsed = 0usize;
        if let Some(data) = dtb {
            if let Some(tree) = fdt::Fdt::new(data) {
                parsed = tree.cpu_count(smp::MAX_CPUS);
                smp::set_possible_cpus(parsed);
            }
        }
        // QEMU `-kernel` passes no DTB, so this is evidence only when a
        // bootloader did hand one over (and the count is non-zero).
        if parsed > 0 && cfg!(feature = "smp-selftest") {
            console::write_str("SMP: fdt cpus=");
            console::write_dec_u64(parsed as u64);
            console::write_str("\r\n");
        }
    }

    // Evidence that the live tree, not the static fallback, supplied the
    // console UART. Printed only when a DTB was handed over.
    if let Some((range, irq)) = crate::driver_resources::dtb_uart() {
        console::write_str("DRIVER UART=DT base=0x");
        console::write_hex_u64(range.base);
        console::write_str(" size=0x");
        console::write_hex_u64(range.size);
        if let Some(intid) = irq {
            console::write_str(" irq=");
            console::write_dec_u64(u64::from(intid));
        }
        if let Some(hz) = crate::driver_resources::uart_clock() {
            console::write_str(" clk=");
            console::write_dec_u64(u64::from(hz));
        }
        console::write_str("\r\n");
    }
    // Evidence that the interrupt controller, like the console, came from the
    // tree description rather than from a built-in table.
    if crate::driver_resources::gic_bound_from_tree() {
        if let Some((distributor, cpu_interface)) = crate::driver_resources::gic_bases() {
            console::write_str("DRIVER GIC=DT dist=0x");
            console::write_hex_u64(distributor);
            console::write_str(" cpu=0x");
            console::write_hex_u64(cpu_interface);
            console::write_str("\r\n");
        }
    }
    if let Some(intid) = crate::driver_resources::timer_intid() {
        console::write_str("DRIVER TIMER=DT intid=");
        console::write_dec_u64(u64::from(intid));
        console::write_str("\r\n");
    }

    #[cfg(feature = "fog")]
    {
        fog::log_banner(&boot_params, &fog_info);
        console::write_str("SILVER MMU OK\r\n");
        if !fog_info.valid || fog_info.memory.is_empty() {
            console::write_str("SILVER FOG HALT: no proven frame-allocator window\r\n");
            loop {
                core::hint::spin_loop();
            }
        }
    }
    ram_log::log("MMU OK");

    let memory_map = memory::current();
    if !memory_map.is_valid() || memory_map.available.is_empty() {
        #[cfg(feature = "fog")]
        console::write_str("SILVER FOG HALT: invalid memory map\r\n");
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }
    let available_pages = frame_allocator::init(memory_map.available);

    if !memory_map.is_valid() || available_pages == 0 || !frame_allocator::run_self_test() {
        exceptions::silver_handle_exception(6, 0, 0, 0);
    }
    ram_log::log("FRAME ALLOC OK");
    simplefb_boot();

    // The heap comes before the filesystem: mounting validates the image with
    // `sfs_format::fsck`, which now tracks one bit per data block (a 16 MiB
    // rootfs has more blocks than a single file's `MAX_FILE_BLOCKS` window and
    // no longer fits a fixed stack array).
    let Some(heap_base) = allocator::init() else {
        exceptions::silver_handle_exception(7, 0, 0, 0);
    };

    let value = alloc::boxed::Box::new(42_u64);
        #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
    {
        let value_address = (&*value as *const u64) as usize;
        console::write_str("HEAP=OK BASE=");
        console::write_hex_u64(heap_base as u64);
        console::write_str(" VALUE=");
        console::write_hex_u64(*value);
        console::write_str(" ADDR=");
        console::write_hex_u64(value_address as u64);
        console::write_str("\r\n");
        boot_metrics::log_milestone("Heap init");
    }
    ram_log::log("HEAP OK");

    // Mount the on-disk SFS in streaming mode: the kernel owns virtio-blk and
    // reads 512-byte blocks on demand through a bounded cache instead of
    // loading the whole image into RAM. Additive: a missing device leaves the
    // boot path unchanged.
    if sfs::load() {
        sfs::report();
        ram_log::log("KSFS loaded");
    }

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

    drop(value);
    #[cfg(feature = "fog")]
    {
        console::write_str("SILVER FRAME ALLOCATOR OK\r\nSILVER HEAP OK base=0x");
        console::write_hex_u64(heap_base as u64);
        console::write_str("\r\n");
    }

    // The tablet/fog path shares the QEMU boot sequence from here on: gic::init
    // dispatches to gicv3, and the timer PPI is selected per platform (INTID
    // 30 on SM8650, 18 on SM6225/khaje).
    // The interrupt controller is bound from the same device map as the
    // console: a GICv2 gets the distributor and CPU-interface bases the tree
    // describes (they are not at a fixed offset from each other on real
    // controllers), a GICv3 keeps its system-register path.
    if let Some(intid) = crate::driver_resources::timer_intid() {
        gic::set_timer_irq(intid);
    }
    match crate::driver_resources::gic_bases() {
        Some((distributor, cpu_interface)) => gic::init_at(distributor, cpu_interface),
        None => gic::init(),
    }
    timer::init();
    #[cfg(feature = "fog-auto-reset")]
    fog::arm_auto_reset(30);
    // BISECT: isolated timer test — unmask IRQs in kernel context, wait for
    // ticks only when a redistributor was confirmed. Without one, spinning
    // for an interrupt that cannot be delivered would just hide the failure.
    #[cfg(any(not(feature = "fog"), feature = "fog-timer-test"))]
    {
        if timer::available() {
            let saved = interrupts::save_and_disable_irq();
            interrupts::enable_irq();
            let mut spin: u32 = 0;
            while timer::ticks() < 30 && spin < 800_000_000 {
                spin += 1;
                core::hint::spin_loop();
            }
            interrupts::restore_irq(saved);
            console::write_str("TIMER TEST: ticks=");
            console::write_dec_u64(timer::ticks());
            console::write_str(" spins=");
            console::write_dec_u64(spin as u64);
            console::write_str("\r\n");
        } else {
            console::write_str("TIMER TEST: SKIP (no confirmed GICv3 timer PPI)\r\n");
        }
    }
    #[cfg(all(feature = "fog", not(feature = "fog-timer-test")))]
    {
        // The first hardware boot must not deliver a timer IRQ before
        // scheduler::init_fog(): gicv3::handle_irq calls schedule() on every
        // tick, and the scheduler state is not ready yet.
        console::write_str("TIMER TEST: SKIP (fog hardware default)\r\n");
    }
    ram_log::log("GICv3+TIMER OK");
    vfs::init();
    seed::seed();
    // The kernel streams SFS reads from virtio-blk. Mutations are disabled in
    // this slice; /tmp is the writable filesystem.
    if sfs::loaded() {
        let _ = vfs::mkdir("/", "/sfs");
        if vfs::mount("/", "/sfs", "sfs", "", 0, |_| false).is_ok() {
            ram_log::log("KSFS mounted");
            // Service state belongs in RAM, not on the image: OpenRC writes
            // `/run/openrc` and `/var/log` on every boot.
            for point in ["/sfs/run", "/sfs/var"] {
                let _ = vfs::mkdir("/", point);
                if vfs::mount("/", point, "ramfs", "", 0, |_| false).is_ok() {
                    ram_log::log("service ramfs");
                }
            }
            // Install the app bundles on first boot (or after a size change).
            seed::seed_apps();
            // `--gpu` attaches a virtio-gpu; the driver sets up a 2D scanout
            // and draws a test pattern when the device is present.
            if virtio_gpu::init() {
                #[cfg(feature = "linux-abi")]
                drm::report();
            }
        }
    }
    #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
    vfs::run_self_test();
    ram_log::log("VFS+SEED OK");
    if silver_microkernel::bundle_sig::signatures_enabled() {
        console::write_str("BUNDLE SIGNATURES: ENFORCED\r\n");
    } else {
        console::write_str("BUNDLE SIGNATURES: none (integrity only)\r\n");
    }

    #[cfg(feature = "fog")]
    {
        console::write_str("SILVER PLATFORM READY\r\n");
        scheduler::init_fog();
        console::write_str("SILVER SCHEDULER START\r\n");
        scheduler::start();
    }
    #[cfg(not(feature = "fog"))]
    {
        scheduler::init();

        #[cfg(not(feature = "tablet"))]
        {
            console::write_str("IRQ READY\r\n");
            for &byte in b"HELLO, WORLD!\r\n" {
                console::write_byte(byte);
            }
            boot_metrics::log_milestone("Scheduler start");
            console::write_str("SCHEDULER START\r\n");
        }
        ram_log::log("SCHEDULER START - BOOT COMPLETE");
        scheduler::start();
    }
}

#[cfg(not(feature = "std"))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Tablet: no console output at all — the line number and message go into
    // the ramoops log; a stray UART write here could abort before logging.
    #[cfg(feature = "tablet")]
    {
        crate::ram_log::log("PANIC");
        if let Some(location) = info.location() {
            crate::ram_log::log_raw(" line=");
            crate::ram_log::log_hex(location.line() as u64);
            crate::ram_log::log_raw("\n");
        }
    }
    #[cfg(not(feature = "tablet"))]
    {
        if let Some(location) = info.location() {
            console::write_str("PANIC AT ");
            console::write_str(location.file());
            console::write_str(":");
            console::write_dec_u64(location.line() as u64);
            console::write_str("\r\n");
        }
        if let Some(message) = info.message().as_str() {
            console::write_str("PANIC MSG ");
            console::write_str(message);
            console::write_str("\r\n");
        }
    }
    exceptions::silver_handle_exception(4, 0, 0, 0)
}

#[cfg(feature = "std")]
fn main() {}
