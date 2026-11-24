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
mod pci_boot;
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
    boot::console::init();

    // PCIe is described by the tree, never guessed from the board. This runs
    // before storage so the first hardware log names the host bridge and any
    // endpoint (notably RP1) that the kernel can see.
    #[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
    {
        let params = boot::get();
        let dtb = params.as_ref().and_then(fog::dtb_slice);
        pci_boot::init(dtb);
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

    // Only the fog banner prints the heap base after the self-tests; the QEMU
    // path reports it at allocation time inside `boot::mem`.
    #[allow(unused_variables)]
    let (heap_base, heap_probe) = boot::mem::init();

    // Mount the on-disk SFS in streaming mode: the kernel owns virtio-blk and
    // reads 512-byte blocks on demand through a bounded cache instead of
    // loading the whole image into RAM. Additive: a missing device leaves the
    // boot path unchanged.
    if sfs::load() {
        sfs::report();
        ram_log::log("KSFS loaded");
    }

    boot::selftest::run();

    drop(heap_probe);
    #[cfg(feature = "fog")]
    {
        console::write_str("SILVER FRAME ALLOCATOR OK\r\nSILVER HEAP OK base=0x");
        console::write_hex_u64(heap_base as u64);
        console::write_str("\r\n");
    }

    boot::interrupts::init();
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

    boot::start::run()
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