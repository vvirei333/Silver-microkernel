//! Redmi 10C (SM6225 "fog") platform bring-up.
//!
//! This module is intentionally conservative: every value it uses comes either
//! from the live FDT passed by the bootloader or from a build-time constant
//! that is clearly marked as an assumption. It never invents a memory range
//! and never asserts an unverified hardware address.

use crate::boot::BootParams;
use crate::fdt::{self, Fdt, Region};

/// Debug-only colour ladder on the Rain/Fog cont_splash framebuffer.
/// `fog-paint` is not part of a normal build; it assumes the live Rain
/// reserved-memory base at 0x5C00_0000 and exists only to make a UART-less
/// unit observable. The boot stub paints green at entry and blue after the
/// placement decision; this function paints later kernel milestones.
#[cfg(feature = "fog-paint")]
pub fn paint(color: u32) {
    const BASE: usize = 0x5C00_0000;
    const SIZE: usize = 0x0080_0000;
    let pointer = crate::memory::phys_to_virt(BASE) as *mut u32;
    let words = SIZE / 4;
    let mut index = 0;
    while index < words {
        unsafe { core::ptr::write_volatile(pointer.add(index), color) };
        index += 1;
    }
}

#[cfg(not(feature = "fog-paint"))]
#[inline(always)]
pub fn paint(_color: u32) {}

/// Paint only the top band of the splash buffer, leaving the last full-screen
/// stage colour visible. Used by the exception handler so a fault colour does
/// not hide the stage that faulted.
#[cfg(feature = "fog-paint")]
pub fn paint_band(color: u32) {
    const BASE: usize = 0x5C00_0000;
    // A thin 16 KiB strip (about six 720-pixel rows) keeps the exception
    // colour visible without hiding the register table painted below it.
    const SIZE: usize = 0x0000_4000;
    let pointer = crate::memory::phys_to_virt(BASE) as *mut u32;
    let words = SIZE / 4;
    let mut index = 0;
    while index < words {
        unsafe { core::ptr::write_volatile(pointer.add(index), color) };
        index += 1;
    }
}

#[cfg(not(feature = "fog-paint"))]
#[inline(always)]
pub fn paint_band(_color: u32) {}

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
    // --- Diagnostics: what the live tree actually said (read-only). ---
    /// INTID passed to the GIC driver for the non-secure physical timer.
    pub timer_irq: u32,
    /// True when `timer_irq` came from the `/timer` node, false when it is the
    /// compile-time khaje default. The bring-up must know which one it used.
    pub timer_from_fdt: bool,
    pub gic_dist_base: u64,
    pub gic_redist_base: u64,
    pub gic_stride: u64,
    pub gic_redist_size: u64,
    /// Raw cells of the timer node's `interrupts` and the GIC node's `reg`.
    pub timer_node: bool,
    pub timer_cells_len: usize,
    pub timer_cells: [u32; crate::fdt::RAW_CELLS],
    pub gic_node: bool,
    pub gic_cells_len: usize,
    pub gic_cells: [u32; crate::fdt::RAW_CELLS],
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
            timer_irq: 0,
            timer_from_fdt: false,
            gic_dist_base: 0,
            gic_redist_base: 0,
            gic_stride: 0,
            gic_redist_size: 0,
            timer_node: false,
            timer_cells_len: 0,
            timer_cells: [0; crate::fdt::RAW_CELLS],
            gic_node: false,
            gic_cells_len: 0,
            gic_cells: [0; crate::fdt::RAW_CELLS],
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
    let timer_node_irq = tree.timer_irq();
    let timer_raw = tree.timer_raw();
    result.timer_node = timer_raw.found;
    result.timer_cells_len = timer_raw.len;
    result.timer_cells = timer_raw.cells;
    result.timer_from_fdt = timer_node_irq.is_some();
    let timer = timer_node_irq.unwrap_or(default_timer);
    result.timer_irq = timer;
    let gic = tree.gic();
    let gic_raw = tree.gic_raw();
    result.gic_node = gic_raw.found;
    result.gic_cells_len = gic_raw.len;
    result.gic_cells = gic_raw.cells;
    result.gic_present = gic.is_some();
    result.gic_v3 = gic
        .map(|info| info.kind == crate::fdt::GicKind::V3)
        .unwrap_or(false);
    if let Some(gic) = gic {
        result.gic_dist_base = gic.distributor;
        result.gic_redist_base = gic.second;
        result.gic_stride = gic.stride;
        result.gic_redist_size = gic.second_size;
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
    // `/reserved-memory` carve-outs fail closed too: an overflowing or
    // malformed `reg` must not be silently dropped from the exclusion list.
    let reserved_result = tree.reserved_regions(&mut reserved);
    let mut reserved_count = reserved_result.unwrap_or(0);
    if reserved_count > 0 {
        result.reserved_first = reserved[0];
    }
    let reserved_slots = reserved.len().saturating_sub(reserved_count);
    // A malformed or overflowing memreserve map must fail the boot closed:
    // dropping an entry would let the planner hand out reserved memory.
    let memreserve = tree.memreserve_regions(&mut reserved[reserved_count..]);
    let memreserve_count = memreserve.unwrap_or(0);
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
        || memreserve_count == reserved_slots
        || reserved_result.is_none()
        || memreserve.is_none();

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

    // The relocation record is written before the copy, so this line only
    // appears when relocation was actually attempted. On a refused attempt
    // the kernel never reaches this code; the source-image copy of the record
    // is then the only evidence (debugger/ramdump, not a user-visible channel).
    if let Some(record) = crate::boot::relocation_record() {
        if record.flags & crate::boot::RELOC_FLAG_ATTEMPTED != 0 {
            console("SILVER RELOC reason=");
            crate::console::write_dec_u64(u64::from(record.reason));
            console(" el=");
            crate::console::write_dec_u64(u64::from(record.entry_el));
            console(" dest=0x");
            crate::console::write_hex_u64(record.dest_start);
            console(" end=0x");
            crate::console::write_hex_u64(record.dest_end);
            console(" size=0x");
            crate::console::write_hex_u64(record.image_size);
            console(" flags=0x");
            crate::console::write_hex_u64(u64::from(record.flags));
            console(" checks=0x");
            crate::console::write_hex_u64(u64::from(record.checks));
            console(" ctr=0x");
            crate::console::write_hex_u64(if record.flags & crate::boot::RELOC_FLAG_CTR_READ != 0 {
                record.ctr_el0
            } else {
                0
            });
            console(" hcr=0x");
            crate::console::write_hex_u64(if record.flags & crate::boot::RELOC_FLAG_HCR_READ != 0 {
                record.hcr_el2
            } else {
                0
            });
            console("\r\n");
        }
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

pub(crate) fn dtb_slice<'a>(params: &BootParams) -> Option<&'a [u8]> {
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
