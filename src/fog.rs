//! Redmi 10C (SM6225 "fog") platform bring-up.
//!
//! This module is intentionally conservative: every value it uses comes either
//! from the live FDT passed by the bootloader or from a build-time constant
//! that is clearly marked as an assumption. It never invents a memory range
//! and never asserts an unverified hardware address.

use crate::boot::BootParams;
use crate::fdt::{self, Fdt, Region};

pub const FOG_COMPATIBLE: [&str; 2] = ["qcom,khaje", "qcom,sm6225"];

/// Everything the kernel learned from the live tree. Zeroed fields mean "not
/// provided", never "known to be zero".
#[derive(Clone, Copy)]
pub struct FogInfo {
    pub valid: bool,
    pub memory: Region,
    pub reserved_count: usize,
    pub reserved_first: Region,
    pub memreserve_first: Region,
    pub dtb_region: Region,
    pub uart_pa: u64,
    pub uart_debug: bool,
    pub uart_kind: crate::fdt::UartKind,
    pub uart_irq: u32,
    pub gic_present: bool,
    pub gic_v3: bool,
    pub boot_cpuid: u32,
    pub fdt_size: usize,
    pub have_fdt: bool,
    pub model: [u8; 64],
    pub model_len: usize,
    pub compatible: [u8; 64],
    pub compatible_len: usize,
}

impl FogInfo {
    const fn empty() -> Self {
        Self {
            valid: false,
            memory: Region { start: 0, end: 0 },
            reserved_count: 0,
            reserved_first: Region { start: 0, end: 0 },
            memreserve_first: Region { start: 0, end: 0 },
            dtb_region: Region { start: 0, end: 0 },
            uart_pa: 0,
            uart_debug: false,
            uart_kind: crate::fdt::UartKind::Geni,
            uart_irq: 0,
            gic_present: false,
            gic_v3: false,
            boot_cpuid: 0,
            fdt_size: 0,
            have_fdt: false,
            model: [0; 64],
            model_len: 0,
            compatible: [0; 64],
            compatible_len: 0,
        }
    }

    pub fn model_str(&self) -> &str {
        core::str::from_utf8(&self.model[..self.model_len]).unwrap_or("?")
    }

    pub fn compatible_str(&self) -> &str {
        core::str::from_utf8(&self.compatible[..self.compatible_len]).unwrap_or("?")
    }
}

impl Default for FogInfo {
    fn default() -> Self {
        Self::empty()
    }
}

static mut INFO: FogInfo = FogInfo::empty();

pub fn info() -> FogInfo {
    unsafe { core::ptr::read(core::ptr::addr_of!(INFO)) }
}

fn set_info(value: FogInfo) {
    unsafe {
        core::ptr::write(core::ptr::addr_of_mut!(INFO), value);
    }
}

/// Probes the live FDT and records the platform facts. Runs after the boot
/// stub enabled the high-half mapping but before the console exists, so it
/// must not print. Returns the info it recorded; `valid == false` means the
/// caller must stop instead of guessing a memory map.
pub fn probe(params: &BootParams) -> FogInfo {
    let mut result = FogInfo::default();

    if !params.has_dtb() {
        set_info(result);
        return result;
    }

    let Some(data) = dtb_slice(params) else {
        set_info(result);
        return result;
    };
    let Some(tree) = Fdt::new(data) else {
        set_info(result);
        return result;
    };

    result.have_fdt = true;
    result.fdt_size = tree.total_size();
    result.boot_cpuid = tree.boot_cpuid();

    let compatible = tree.root_compatible().unwrap_or("");
    let all_compatible = tree.root_compatible_all().unwrap_or(compatible);
    let is_fog = FOG_COMPATIBLE
        .iter()
        .any(|needle| compatible_has(all_compatible, needle));
    if !is_fog {
        set_info(result);
        return result;
    }
    crate::platform::force(crate::platform::Kind::Fog);

    if let Some(model) = tree.root_model() {
        copy_str(&mut result.model, &mut result.model_len, model);
    }
    copy_str(
        &mut result.compatible,
        &mut result.compatible_len,
        compatible,
    );

    // The node scan is authoritative for both the base and the programming
    // model. `stdout-path` is logged by platform tooling but is not trusted
    // over a real UART node: an alias or a stale path must not select an
    // address that no driver can program.
    let default_uart_irq = if cfg!(feature = "fog") { 363 } else { 33 };
    if let Some(uart) = tree.find_uart() {
        result.uart_pa = uart.base;
        result.uart_debug = uart.debug;
        result.uart_kind = uart.kind;
        result.uart_irq = uart.irq.unwrap_or(default_uart_irq);
    }

    // The interrupt controller is the other half of "the kernel keeps
    // running": without a working timer there is no preemption and no sleep.
    // The live tree names the GIC generation and its bases; only a V3 tree is
    // accepted here (khaje is V3). A V2 tree is left to the compile-time
    // defaults rather than being silently treated as V3.
    let default_timer = if cfg!(feature = "fog") { 18 } else { 30 };
    let timer = tree.timer_irq().unwrap_or(default_timer);
    let gic = tree.gic();
    result.gic_present = gic.is_some();
    result.gic_v3 = gic
        .map(|info| info.kind == crate::fdt::GicKind::V3)
        .unwrap_or(false);
    if let Some(gic) = gic {
        if gic.kind == crate::fdt::GicKind::V3 {
            crate::gicv3::configure(
                gic.distributor as usize,
                gic.second as usize,
                gic.stride as usize,
                gic.second_size as usize,
                timer,
                result.uart_irq,
            );
        }
    }

    // Collect the live memory and reservation facts. The fixed array bounds
    // are deliberately generous for one SoC and the parser fails closed when a
    // tree has more entries than we can reason about.
    let mut memory = [Region::default(); 16];
    let memory_count = tree.memory_regions(&mut memory);
    let mut reserved = [Region::default(); 64];
    let mut reserved_count = tree.reserved_regions(&mut reserved);
    if reserved_count > 0 {
        result.reserved_first = reserved[0];
    }
    let reserved_slots = reserved.len().saturating_sub(reserved_count);
    let memreserve_count = tree.memreserve_regions(&mut reserved[reserved_count..]);
    if memreserve_count > 0 {
        result.memreserve_first = reserved[reserved_count];
    }
    reserved_count += memreserve_count;

    // The fixed arrays are deliberately larger than a normal khaje tree. If
    // one is full, the parser may have dropped entries, so the placement
    // decision would be based on an incomplete exclusion list. Fail closed
    // instead of handing out a range that might be reserved.
    let truncated = memory_count == memory.len()
        || reserved_count == reserved.len()
        || memreserve_count == reserved_slots;

    // Exclusions: reservations, the loaded image (including its BSS and
    // stack), and the DTB itself. `image_size` is the span ABL was told to
    // reserve, so the whole span is excluded, not just the file bytes.
    let mut excluded = [Region::default(); 80];
    let mut excluded_count = 0usize;
    for &region in &reserved[..reserved_count] {
        excluded[excluded_count] = region;
        excluded_count += 1;
    }
    // Everything below the image is ABL-owned or simply unproven; never let
    // the planner return a window there even if it happens to be larger.
    excluded[excluded_count] = Region::new(crate::memory::DRAM_BASE as u64, params.image_base);
    excluded_count += 1;
    let image_start = params.image_base;
    let image_end = image_start.saturating_add(params.image_size);
    excluded[excluded_count] = Region::new(image_start, image_end);
    excluded_count += 1;
    let dtb_start = params.dtb_pa;
    let dtb_end = dtb_start.saturating_add(params.dtb_size);
    result.dtb_region = Region::new(dtb_start, dtb_end);
    excluded[excluded_count] = Region::new(dtb_start, dtb_end);
    excluded_count += 1;

    // The boot page tables map exactly one gigabyte at DRAM_BASE. A larger
    // bank in the live tree (0x8000_0000 and above) is real RAM but is not
    // addressable by this first PoC, so clip it out before planning rather
    // than letting `MemoryMap::is_valid` reject the whole boot.
    let mapped_limit = Region::new(
        crate::memory::DRAM_BASE as u64,
        crate::memory::DRAM_END as u64,
    );
    let mut mapped_memory = [Region::default(); 16];
    let mapped_count = fdt::clip_regions(&memory[..memory_count], mapped_limit, &mut mapped_memory);

    let window = fdt::select_allocator_window(
        &mapped_memory[..mapped_count],
        &mut excluded[..excluded_count],
        crate::memory::PAGE_SIZE as u64,
        1024, // 4 MiB minimum: the 2 MiB heap plus VFS/task headroom.
    );

    result.memory = match window {
        Some(region) => Region::new(region.start, region.end),
        None => Region::default(),
    };
    result.reserved_count = reserved_count;
    // A V2 interrupt controller would need the GICv2 runtime path, which the
    // khaje port does not implement. Fail closed instead of programming a V3
    // frame at a V2 address.
    result.valid = !truncated && !result.memory.is_empty() && result.gic_v3;
    set_info(result);
    result
}

/// Prints the evidence the probe collected. Called only after the console is
/// initialised; on a UART-less unit this produces nothing, which is honest.
pub fn log_banner(params: &BootParams, info: &FogInfo) {
    let console = crate::console::write_str;
    console("SILVER FOG ENTRY\r\n");
    console("SILVER BOOT PARAMS ");
    if params.magic == crate::boot::BOOT_MAGIC {
        console("OK pc=0x");
        crate::console::write_hex_u64(params.actual_pc);
        console(" el=");
        crate::console::write_dec_u64(params.entry_el >> 2);
        console(" sctlr=0x");
        crate::console::write_hex_u64(params.sctlr);
        console(" mpidr=0x");
        crate::console::write_hex_u64(params.mpidr);
        console(" delta=0x");
        crate::console::write_hex_u64(params.load_delta as u64);
        console("\r\n");
    } else {
        console("MISSING\r\n");
    }

    console("SILVER FDT ");
    if info.have_fdt {
        console("OK size=0x");
        crate::console::write_hex_u64(info.fdt_size as u64);
        console(" model=");
        console(info.model_str());
        console(" compatible=");
        console(info.compatible_str());
        console("\r\n");
    } else {
        console("MISSING/UNMAPPED\r\n");
    }

    console("SILVER MEMORY MAP ");
    if info.memory.is_empty() {
        console("UNPROVEN\r\n");
    } else {
        console("OK base=0x");
        crate::console::write_hex_u64(info.memory.start);
        console(" size=0x");
        crate::console::write_hex_u64(info.memory.size());
        console(" reserved=");
        crate::console::write_dec_u64(info.reserved_count as u64);
        console(" resv=0x");
        crate::console::write_hex_u64(info.reserved_first.start);
        console("+0x");
        crate::console::write_hex_u64(info.reserved_first.size());
        console(" memresv=0x");
        crate::console::write_hex_u64(info.memreserve_first.start);
        console("+0x");
        crate::console::write_hex_u64(info.memreserve_first.size());
        console(" dtb=0x");
        crate::console::write_hex_u64(info.dtb_region.start);
        console("+0x");
        crate::console::write_hex_u64(info.dtb_region.size());
        console("\r\n");
    }

    console("SILVER GIC ");
    if info.gic_v3 {
        console("OK kind=gicv3\r\n");
    } else if info.gic_present {
        console("UNSUPPORTED kind=gicv2\r\n");
    } else {
        console("MISSING\r\n");
    }

    console("SILVER UART ");
    if info.uart_pa != 0 {
        console("OK base=0x");
        crate::console::write_hex_u64(info.uart_pa);
        console(if info.uart_debug { " debug=1" } else { " debug=0" });
        console(match info.uart_kind {
            crate::fdt::UartKind::Geni => " kind=geni",
            crate::fdt::UartKind::Pl011 => " kind=pl011",
        });
        console("\r\n");
    } else {
        console("MISSING\r\n");
    }
}

fn copy_str(destination: &mut [u8; 64], length: &mut usize, source: &str) {
    let bytes = source.as_bytes();
    let count = bytes.len().min(destination.len());
    destination[..count].copy_from_slice(&bytes[..count]);
    *length = count;
}

fn dtb_slice<'a>(params: &BootParams) -> Option<&'a [u8]> {
    let start = params.dtb_pa;
    let size = params.dtb_size;
    if start == 0 || size == 0 {
        return None;
    }
    // The boot tables map the first DRAM gigabyte; a DTB outside it cannot be
    // read without inventing a mapping, so fail closed instead.
    let end = start.checked_add(size)?;
    if start < crate::memory::DRAM_BASE as u64 || end > crate::memory::DRAM_END as u64 {
        return None;
    }
    let pointer = crate::memory::phys_to_virt(start as usize) as *const u8;
    Some(unsafe { core::slice::from_raw_parts(pointer, size as usize) })
}

fn compatible_has(compatible: &str, needle: &str) -> bool {
    // Device-tree board strings carry suffixes: `qcom,khaje-idp`,
    // `qcom,khaje-qrd`. Match the family prefix on a component boundary,
    // never an arbitrary substring (`qcom,khajexyz` must not match).
    compatible.split('\0').any(|entry| {
        entry == needle
            || (entry.starts_with(needle)
                && entry.as_bytes().get(needle.len()) == Some(&b'-'))
    })
}
