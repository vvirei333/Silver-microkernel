//! Console milestone: bind the devices the tree names, then start the console.
//!
//! The order is forced by the hardware hand-over: on a new machine the device
//! tree is what names the UART, so the console cannot be selected first. All
//! of it runs after the MMU is on (the parser reads through `phys_to_virt`).

use crate::boot;
#[cfg(not(feature = "tablet"))]
use crate::cache;
#[cfg(feature = "gdb-stub")]
use crate::gdb;
#[cfg(all(not(feature = "tablet"), not(feature = "fog")))]
use crate::{boot_metrics, fdt, smp};
use crate::{console, driver_resources, fog};

/// Binds the driver resources from the live tree, brings the console up and
/// reports what the tree supplied. Every `DRIVER ...=DT` line is evidence that
/// the description, not the built-in fallback table, chose the device.
pub fn init() {
    let params = boot::get();
    let dtb = params.as_ref().and_then(fog::dtb_slice);
    // The PSCI conduit belongs to the firmware, so read it from the same tree
    // that names the UART before any power or AP bring-up can issue a call.
    if let Some(dtb) = dtb {
        crate::power::init(dtb);
    }
    driver_resources::init(dtb);
    #[cfg(feature = "gdb-stub")]
    gdb::probe_transport(dtb);
    // Console first after that: a bad memory map must be *reported* before the
    // allocator asserts on an empty window.
    console::init();
    // Wall-clock epoch for CLOCK_REALTIME. The PL031 is optional: a tree
    // without the node leaves it unavailable and the clock falls back to
    // the monotonic tick count.
    if crate::rtc::init(dtb) {
        if let Some((seconds, _)) = crate::rtc::now() {
            console::write_str("RTC=PL031 epoch=");
            console::write_dec_u64(seconds);
            console::write_str("\r\n");
        }
    }
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
    if let Some((range, irq)) = driver_resources::dtb_uart() {
        console::write_str("DRIVER UART=DT base=0x");
        console::write_hex_u64(range.base);
        console::write_str(" size=0x");
        console::write_hex_u64(range.size);
        if let Some(intid) = irq {
            console::write_str(" irq=");
            console::write_dec_u64(u64::from(intid));
        }
        if let Some(hz) = driver_resources::uart_clock() {
            console::write_str(" clk=");
            console::write_dec_u64(u64::from(hz));
        }
        console::write_str("\r\n");
    }
    // Evidence that the interrupt controller, like the console, came from the
    // tree description rather than from a built-in table.
    if driver_resources::gic_bound_from_tree() {
        if let Some((distributor, cpu_interface)) = driver_resources::gic_bases() {
            console::write_str("DRIVER GIC=DT dist=0x");
            console::write_hex_u64(distributor);
            console::write_str(" cpu=0x");
            console::write_hex_u64(cpu_interface);
            console::write_str("\r\n");
        }
    }
    if let Some(intid) = driver_resources::timer_intid() {
        console::write_str("DRIVER TIMER=DT intid=");
        console::write_dec_u64(u64::from(intid));
        console::write_str("\r\n");
    }
}
