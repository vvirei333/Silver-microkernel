//! Minimal flattened-device-tree (FDT / DTB) parser and boot-memory planner.
//!
//! This module is deliberately free of kernel state: the same code is compiled
//! into the bare-metal kernel and into the host test binary (`cargo test
//! --lib`). It never allocates and never trusts the blob: every offset is
//! bounds-checked and every size is checked for overflow, so a corrupted or
//! hostile DTB can only make a query return `None`, never fault the kernel.
//!
//! Only the subset the first Redmi 10C PoC needs is implemented:
//!
//! * the root identity (`model`, `compatible`, `#address-cells`,
//!   `#size-cells`),
//! * `/memory` regions,
//! * `/reserved-memory` children and the header memory-reservation map,
//! * `/chosen` (`bootargs`, `stdout-path`),
//! * the debug-UART node (`compatible` contains `qcom,geni-uart`), and
//! * a conservative planner that picks one contiguous free physical window
//!   for the frame allocator.
//!
//! Everything else in the live tree is ignored. The parser is not a general
//! libfdt replacement.

use core::str;

use crate::pci::{PciHost, PciHostKind};

/// FDT magic, big-endian in the blob.
pub const FDT_MAGIC: u32 = 0xd00d_feed;
const HEADER_SIZE: usize = 40;
const MAX_CELLS: u32 = 2;
const MAX_PATH: usize = 512;
const MAX_DEPTH: usize = 32;

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// A half-open physical region `[start, end)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
}

impl Region {
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    pub const fn size(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }

    pub const fn contains(self, address: u64) -> bool {
        address >= self.start && address < self.end
    }

    pub const fn overlaps(self, other: Region) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Intersection, or an empty region when they do not overlap.
    pub const fn intersect(self, other: Region) -> Region {
        let start = if self.start > other.start {
            self.start
        } else {
            other.start
        };
        let end = if self.end < other.end {
            self.end
        } else {
            other.end
        };
        Region::new(start, end)
    }
}

/// Which UART programming model the node describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UartKind {
    /// Qualcomm GENI SE (SM8650/SM6225).
    Geni,
    /// ARM PL011 (QEMU `virt`).
    Pl011,
}

/// The information the kernel needs from one UART node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UartNode {
    /// Physical base address taken from the node's unit address (`serial@4a9000`)
    /// or from its `reg` property.
    pub base: u64,
    /// `true` for `qcom,geni-debug-uart` (preferred over a plain `geni-uart`).
    pub debug: bool,
    pub kind: UartKind,
    /// INTID from the node's `interrupts` property (SPI 32 + n, PPI 16 + n).
    pub irq: Option<u32>,
    /// `clock-frequency` in Hz, when the node publishes it (directly or through
    /// the clock node its `clocks` property points at).
    ///
    /// A UART's baud divisor is a function of its reference clock, so a board
    /// whose UART runs at 48 MHz needs a different divisor from one at 24 MHz.
    /// Reading it here is what keeps `pl011` from assuming QEMU's clock.
    pub clock: Option<u32>,
    /// First `clocks` cell: the phandle of the clock provider, if named.
    ///
    /// `find_uart` resolves it with [`Fdt::clock_rate_of`] after the walk, which
    /// is how Raspberry Pi trees express the UART clock.
    pub clock_phandle: Option<u32>,
}

/// A driver bindable from the live tree. The kernel's driver table maps this
/// kind to the EL0 program that owns the node; the spawn path grants exactly
/// this MMIO range and INTID.
///
/// Defined in the shared library (`src/devices.rs`) so the "which driver serves
/// which role" policy can be tested without a machine.
pub use crate::devices::DriverKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriverNode {
    pub kind: DriverKind,
    pub mmio: Region,
    pub irq: Option<u32>,
}

/// Nodes one pass may discover. The product has one console UART today; the
/// limit leaves room for a second UART or a simplefb node later.
pub const MAX_DRIVER_NODES: usize = 8;

/// GIC generation declared by the live tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GicKind {
    V2,
    V3,
}

/// PSCI conduit declared by the `/psci` node.
///
/// The conduit is a property of the firmware, not of the CPU vendor: QEMU
/// `virt` exposes PSCI over `hvc`, while the Raspberry Pi firmware uses the
/// secure monitor (`smc`). Guessing from the board name works until the first
/// board whose firmware differs from its SoC family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsciMethod {
    Hvc,
    Smc,
}

/// The register bases needed to bring up the interrupt controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GicInfo {
    pub kind: GicKind,
    /// GIC distributor base (GICD).
    pub distributor: u64,
    /// GICv2 CPU interface base, or GICv3 redistributor base (GICR).
    pub second: u64,
    /// Size of the second region (GICv2 CPU interface or GICv3 GICR window).
    pub second_size: u64,
    /// GICv3 redistributor stride; unused for GICv2.
    pub stride: u64,
}

/// Raw device-tree cells of one node property, captured for diagnostics.
///
/// The bring-up must not assume that the timer INTID or the GIC bases are the
/// ones the vendor DTS suggests: this type keeps the exact big-endian cells so
/// they can be displayed and compared with the parsed values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawFacts {
    pub found: bool,
    /// Number of cells stored (the property may be longer than `cells`).
    pub len: usize,
    pub cells: [u32; RAW_CELLS],
}

impl Default for RawFacts {
    fn default() -> Self {
        Self {
            found: false,
            len: 0,
            cells: [0; RAW_CELLS],
        }
    }
}

pub const RAW_CELLS: usize = 12;

/// A borrowed view of one validated DTB blob.
pub struct Fdt<'a> {
    data: &'a [u8],
    off_struct: usize,
    size_struct: usize,
    off_strings: usize,
    size_strings: usize,
    off_rsvmap: usize,
    totalsize: usize,
    version: u32,
    boot_cpuid: u32,
}

/// Callback interface used by [`Fdt::walk`].
///
/// `'a` is the lifetime of the DTB blob; property values borrow directly from
/// it. Implementors may carry their own independent borrows.
trait Visitor<'a> {
    fn begin(&mut self, path: &str, name: &str);
    fn property(&mut self, path: &str, name: &str, value: &'a [u8]);
    fn end(&mut self, path: &str);
}

impl<'a> Fdt<'a> {
    /// Validates and wraps a DTB. Returns `None` for any structural problem.
    pub fn new(data: &'a [u8]) -> Option<Self> {
        if data.len() < HEADER_SIZE {
            return None;
        }
        if read_be32(data, 0)? != FDT_MAGIC {
            return None;
        }
        let totalsize = read_be32(data, 4)? as usize;
        let off_struct = read_be32(data, 8)? as usize;
        let off_strings = read_be32(data, 12)? as usize;
        let off_rsvmap = read_be32(data, 16)? as usize;
        let version = read_be32(data, 20)?;
        let last_comp_version = read_be32(data, 24)?;
        let boot_cpuid = read_be32(data, 28)?;
        let size_strings = read_be32(data, 32)? as usize;
        let size_struct = read_be32(data, 36)? as usize;

        // Version 17 is the current format; version 16 has the same header.
        // Older layouts are not needed for Qualcomm/Android DTBs.
        if version < 16 || last_comp_version > 17 {
            return None;
        }
        if totalsize < HEADER_SIZE || totalsize > data.len() {
            return None;
        }
        if off_struct % 4 != 0 || size_struct % 4 != 0 {
            return None;
        }
        let struct_end = off_struct.checked_add(size_struct)?;
        let strings_end = off_strings.checked_add(size_strings)?;
        if struct_end > totalsize || strings_end > totalsize {
            return None;
        }
        if off_rsvmap < HEADER_SIZE || off_rsvmap + 16 > totalsize {
            return None;
        }

        Some(Self {
            data: &data[..totalsize],
            off_struct,
            size_struct,
            off_strings,
            size_strings,
            off_rsvmap,
            totalsize,
            version,
            boot_cpuid,
        })
    }

    pub fn total_size(&self) -> usize {
        self.totalsize
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    /// The physical ID of the boot CPU recorded by the bootloader, when set.
    pub fn boot_cpuid(&self) -> u32 {
        self.boot_cpuid
    }

    /// Root `model` property (first string), if present.
    pub fn root_model(&self) -> Option<&'a str> {
        let mut props = RootProps::default();
        self.walk(&mut props)?;
        props.model
    }

    /// Root `compatible` property (first string), if present.
    pub fn root_compatible(&self) -> Option<&'a str> {
        let mut props = RootProps::default();
        self.walk(&mut props)?;
        props.compatible
    }

    /// The complete root `compatible` property, including every NUL-separated
    /// entry. Use this when matching a board family whose node names carry a
    /// suffix (`qcom,khaje-idp` vs `qcom,khaje`).
    pub fn root_compatible_all(&self) -> Option<&'a str> {
        let mut props = RootProps::default();
        self.walk(&mut props)?;
        props.compatible_all
    }

    /// Root `#address-cells` / `#size-cells` (defaults 2/2).
    pub fn root_cells(&self) -> (u32, u32) {
        let mut props = RootProps::default();
        if self.walk(&mut props).is_none() {
            return (2, 2);
        }
        (props.address_cells, props.size_cells)
    }

    /// `/chosen` `bootargs` value, if present.
    pub fn chosen_bootargs(&self) -> Option<&'a str> {
        let mut props = ChosenProps::default();
        self.walk(&mut props)?;
        props.bootargs
    }

    /// `/chosen` `stdout-path` (or the legacy `linux,stdout-path`) value.
    pub fn chosen_stdout(&self) -> Option<&'a str> {
        let mut props = ChosenProps::default();
        self.walk(&mut props)?;
        props.stdout
    }

    /// Copies the `/memory` regions into `out` and returns how many were
    /// written. Zero-sized placeholder nodes (bootloader-filled) are skipped.
    pub fn memory_regions(&self, out: &mut [Region]) -> usize {
        let mut visitor = MemoryRegions {
            regions: out,
            count: 0,
            address_cells: 2,
            size_cells: 2,
            malformed: false,
        };
        if self.walk(&mut visitor).is_none() || visitor.malformed {
            return 0;
        }
        visitor.count
    }

    /// Copies `/reserved-memory` child regions into `out`.
    ///
    /// Fails closed (`None`) on a malformed `reg`, an overflowing
    /// `address + size`, or a carve-out list that does not fit in `out`.
    /// Dropping a reservation would let the allocator hand out memory the
    /// firmware marked as reserved, so a partial answer is never returned.
    pub fn reserved_regions(&self, out: &mut [Region]) -> Option<usize> {
        let mut visitor = ReservedRegions {
            regions: out,
            count: 0,
            address_cells: 2,
            size_cells: 2,
            in_reserved: false,
            depth: 0,
            malformed: false,
        };
        self.walk(&mut visitor)?;
        if visitor.malformed {
            return None;
        }
        Some(visitor.count)
    }

    /// Copies the DTB header memory-reservation map into `out`. This is where
    /// the bootloader normally records the DTB itself and the ramdisk.
    ///
    /// Fails closed (`None`) on a malformed or overflowing entry and when the
    /// map does not fit in `out`: silently dropping a reservation would let
    /// the allocator hand out memory the firmware marked as reserved. The
    /// pre-relocation validator uses the same policy.
    pub fn memreserve_regions(&self, out: &mut [Region]) -> Option<usize> {
        let mut count = 0usize;
        let mut offset = self.off_rsvmap;
        loop {
            let Some(next) = offset.checked_add(16) else {
                return None;
            };
            if next > self.totalsize {
                return Some(count);
            }
            let address = read_be64(self.data, offset)?;
            let size = read_be64(self.data, offset + 8)?;
            if address == 0 && size == 0 {
                return Some(count);
            }
            if size != 0 {
                let end = address.checked_add(size)?;
                if count >= out.len() {
                    return None;
                }
                out[count] = Region::new(address, end);
                count += 1;
            }
            offset = next;
        }
    }

    /// Number of CPU nodes in the live tree, clamped to `limit`. A tree that
    /// does not describe `/cpus` yields 0 and the caller keeps its default.
    pub fn cpu_count(&self, limit: usize) -> usize {
        let mut counter = CpuCounter { count: 0, limit };
        if self.walk(&mut counter).is_none() {
            return 0;
        }
        counter.count
    }

    /// PSCI conduit declared by the root `/psci` node, if the tree has one.
    pub fn psci_method(&self) -> Option<PsciMethod> {
        let mut finder = PsciFinder { method: None };
        self.walk(&mut finder)?;
        finder.method
    }

    /// First enabled node whose `compatible` list contains `compatible`,
    /// returning its first `reg` range.
    ///
    /// This is for platform devices with a simple MMIO window. It does not
    /// translate `ranges`; callers must only use nodes whose `reg` is already
    /// a CPU physical address (the BCM2712 SDHCI node is one).
    pub fn first_enabled_reg(&self, compatible: &str) -> Option<Region> {
        let mut finder = EnabledRegFinder {
            wanted: compatible,
            found: None,
            depth: 0,
            nodes: [CompatibleState::default(); MAX_DEPTH],
        };
        let _ = self.walk(&mut finder);
        finder.found
    }

    /// PCI host bridges the tree enables, in tree order.
    ///
    /// An ECAM host (`pci-host-ecam-generic`) is described by its `reg`
    /// window and `bus-range`. The BCM2712 host uses index/data registers
    /// instead of ECAM, so its `reg` is the controller register block and its
    /// endpoint bus defaults to 1..=2 (`00:00.0` is the root bridge, RP1 is
    /// `01:00.0`). Disabled nodes are never returned.
    pub fn pci_hosts(&self, out: &mut [PciHost]) -> usize {
        let mut finder = PciFinder {
            out,
            count: 0,
            depth: 0,
            nodes: [PciState::default(); MAX_DEPTH],
        };
        let _ = self.walk(&mut finder);
        finder.count
    }

    /// Copies the `reg` values of direct `/cpus/cpu*` children into `out`
    /// and returns how many CPU nodes were seen (clamped to `out.len()`).
    ///
    /// A CPU node's `reg` is its MPIDR, not its index: on a clustered SoC the
    /// second core may be affinity `0x100`, so passing the loop index to PSCI
    /// would power on a nonexistent PE. A node without a usable `reg` leaves
    /// the caller's sentinel in place.
    pub fn cpu_mpidrs(&self, out: &mut [u64]) -> usize {
        let mut finder = CpuMpidrs { out, seen: 0 };
        let _ = self.walk(&mut finder);
        finder.seen.min(finder.out.len())
    }

    /// Finds the best GENI UART node: a `qcom,geni-debug-uart` wins over a
    /// plain `qcom,geni-uart`; `status = "disabled"` nodes are ignored.
    pub fn find_uart(&self) -> Option<UartNode> {
        let mut visitor = UartFinder::default();
        self.walk(&mut visitor)?;
        let mut best = visitor.best?;
        // Boards that name the clock by phandle (Raspberry Pi does:
        // `clocks = <&clk_uart>`) only carry the rate on the provider node, so
        // resolve it here rather than leaving the driver on its default.
        if best.clock.is_none() {
            if let Some(phandle) = best.clock_phandle {
                best.clock = self.clock_rate_of(phandle);
            }
        }
        Some(best)
    }

    /// Rate in Hz published by the clock provider with this `phandle`.
    ///
    /// A provider is a `fixed-clock`/`clock-*` node carrying `clock-frequency`
    /// (or the first `assigned-clock-rates` cell). The same plausibility bound
    /// as [`UartNode::clock`] applies: a wild value must not reach a divisor.
    pub fn clock_rate_of(&self, phandle: u32) -> Option<u32> {
        if phandle == 0 {
            return None;
        }
        let mut finder = ClockFinder {
            wanted: phandle,
            depth: 0,
            nodes: [ClockState::default(); MAX_DEPTH],
            rate: None,
        };
        self.walk(&mut finder)?;
        finder.rate
    }

    /// Every enabled node a known driver can bind, in tree order. The node's
    /// first `reg` pair is the MMIO range; `interrupts` gives the INTID using
    /// the same SPI/PPI encoding as `find_uart`. A node without a plausible
    /// `reg` or with `status = "disabled"` is skipped (dead hardware is not a
    /// crash).
    pub fn driver_nodes(&self, out: &mut [DriverNode]) -> usize {
        let mut finder = DriverFinder {
            out,
            count: 0,
            depth: 0,
            nodes: [DriverState::default(); MAX_DEPTH],
        };
        let _ = self.walk(&mut finder);
        finder.count
    }

    /// The interrupt controller described by the live tree, if present.
    pub fn gic(&self) -> Option<GicInfo> {
        let mut visitor = GicFinder::default();
        self.walk(&mut visitor)?;
        visitor.best
    }

    /// INTID of the non-secure physical timer (the second `arm,armv8-timer`
    /// interrupt), if the tree describes it.
    pub fn timer_irq(&self) -> Option<u32> {
        let mut visitor = TimerFinder::default();
        self.walk(&mut visitor)?;
        visitor.irq
    }

    /// Raw `interrupts` cells of the `arm,armv8-timer` node, in tree order.
    /// Display-only: the parser above remains the single source of truth.
    pub fn timer_raw(&self) -> RawFacts {
        let mut visitor = TimerRawFinder::default();
        let _ = self.walk(&mut visitor);
        visitor.facts
    }

    /// Raw `reg` cells of the GICv3 node, in tree order. Display-only.
    pub fn gic_raw(&self) -> RawFacts {
        let mut visitor = GicRawFinder::default();
        let _ = self.walk(&mut visitor);
        visitor.facts
    }

    /// Resolves a `/chosen/stdout-path` value (with optional `:options`
    /// suffix) to the physical base in the final node's unit address.
    pub fn resolve_stdout_base(&self, stdout: &str) -> Option<u64> {
        let path = stdout.split(':').next().unwrap_or(stdout);
        if path.starts_with('/') {
            return unit_address(path.rsplit('/').next()?);
        }

        // Alias form: look the name up in /aliases and resolve the value.
        let mut visitor = AliasFinder {
            wanted: path,
            value: None,
        };
        self.walk(&mut visitor)?;
        let target = visitor.value?;
        let target = target.split(':').next().unwrap_or(target);
        unit_address(target.rsplit('/').next()?)
    }

    fn walk<V: Visitor<'a>>(&self, visitor: &mut V) -> Option<()> {
        let end = self.off_struct.checked_add(self.size_struct)?;
        let mut offset = self.off_struct;
        let mut path = [0u8; MAX_PATH];
        let mut path_len = 0usize;
        let mut lengths = [0u16; MAX_DEPTH];
        let mut depth = 0usize;

        while offset + 4 <= end {
            let token = read_be32(self.data, offset)?;
            offset += 4;
            match token {
                FDT_BEGIN_NODE => {
                    let name = self.read_cstr(offset, end)?;
                    offset += align4(name.len() + 1);

                    let previous = path_len;
                    if depth == 0 {
                        path[0] = b'/';
                        path_len = 1;
                    } else if path_len < MAX_PATH {
                        if path_len > 0 && path[path_len - 1] != b'/' {
                            path[path_len] = b'/';
                            path_len += 1;
                        }
                        let bytes = name.as_bytes();
                        if path_len + bytes.len() > MAX_PATH {
                            return None;
                        }
                        path[path_len..path_len + bytes.len()].copy_from_slice(bytes);
                        path_len += bytes.len();
                    } else {
                        return None;
                    }
                    if depth >= MAX_DEPTH {
                        return None;
                    }
                    lengths[depth] = previous as u16;
                    depth += 1;

                    let path_str = str::from_utf8(&path[..path_len]).ok()?;
                    visitor.begin(path_str, name);
                }
                FDT_END_NODE => {
                    if depth == 0 {
                        return None;
                    }
                    depth -= 1;
                    let path_str = str::from_utf8(&path[..path_len]).ok()?;
                    visitor.end(path_str);
                    path_len = lengths[depth] as usize;
                }
                FDT_PROP => {
                    if offset + 8 > end {
                        return None;
                    }
                    let length = read_be32(self.data, offset)? as usize;
                    let name_offset = read_be32(self.data, offset + 4)? as usize;
                    offset += 8;
                    let value_end = offset.checked_add(length)?;
                    if value_end > end {
                        return None;
                    }
                    let value = &self.data[offset..value_end];
                    offset += align4(length);

                    if name_offset >= self.size_strings {
                        return None;
                    }
                    let name_start = self.off_strings.checked_add(name_offset)?;
                    let strings_end = self.off_strings.checked_add(self.size_strings)?;
                    let name = self.read_cstr(name_start, strings_end)?;
                    let path_str = str::from_utf8(&path[..path_len]).ok()?;
                    visitor.property(path_str, name, value);
                }
                FDT_NOP => {}
                FDT_END => return Some(()),
                _ => return None,
            }
        }
        None
    }

    fn read_cstr(&self, offset: usize, limit: usize) -> Option<&'a str> {
        // `limit` is the end of the block that owns the string: the struct
        // block for node names, the strings block for property names. Reading
        // past it could pick up a NUL from unrelated data and accept a name
        // that the tree does not actually contain.
        if offset >= limit || limit > self.totalsize {
            return None;
        }
        let remaining = self.data.get(offset..limit)?;
        let end = remaining.iter().position(|&byte| byte == 0)?;
        // Device-tree names are ASCII; reject anything that is not valid UTF-8
        // rather than silently mangling a path.
        str::from_utf8(&remaining[..end]).ok()
    }
}

// ---------------------------------------------------------------------------
// Visitors
// ---------------------------------------------------------------------------

struct RootProps<'a> {
    model: Option<&'a str>,
    compatible: Option<&'a str>,
    compatible_all: Option<&'a str>,
    address_cells: u32,
    size_cells: u32,
}

impl Default for RootProps<'_> {
    fn default() -> Self {
        // Device-tree defaults when the root omits the cell properties.
        Self {
            model: None,
            compatible: None,
            compatible_all: None,
            address_cells: 2,
            size_cells: 2,
        }
    }
}

impl<'a> Visitor<'a> for RootProps<'a> {
    fn begin(&mut self, _path: &str, _name: &str) {}

    fn property(&mut self, path: &str, name: &str, value: &'a [u8]) {
        if path != "/" {
            return;
        }
        match name {
            "model" => self.model = first_string(value),
            "compatible" => {
                self.compatible = first_string(value);
                self.compatible_all = core::str::from_utf8(value).ok();
            }
            "#address-cells" => {
                if let Some(cells) = read_be32(value, 0) {
                    if cells <= MAX_CELLS {
                        self.address_cells = cells;
                    }
                }
            }
            "#size-cells" => {
                if let Some(cells) = read_be32(value, 0) {
                    if cells <= MAX_CELLS {
                        self.size_cells = cells;
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {}
}

#[derive(Default)]
struct ChosenProps<'a> {
    bootargs: Option<&'a str>,
    stdout: Option<&'a str>,
}

impl<'a> Visitor<'a> for ChosenProps<'a> {
    fn begin(&mut self, _path: &str, _name: &str) {}

    fn property(&mut self, path: &str, name: &str, value: &'a [u8]) {
        if path != "/chosen" {
            return;
        }
        match name {
            "bootargs" => {
                if let Some(text) = first_string(value) {
                    self.bootargs = Some(text);
                }
            }
            "stdout-path" | "linux,stdout-path" => {
                if let Some(text) = first_string(value) {
                    self.stdout = Some(text);
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {}
}

struct MemoryRegions<'o> {
    regions: &'o mut [Region],
    count: usize,
    address_cells: u32,
    size_cells: u32,
    malformed: bool,
}

impl<'a, 'o> Visitor<'a> for MemoryRegions<'o> {
    fn begin(&mut self, _path: &str, _name: &str) {}

    fn property(&mut self, path: &str, name: &str, value: &'a [u8]) {
        if path == "/" {
            match name {
                "#address-cells" => {
                    if let Some(cells) = read_be32(value, 0) {
                        if cells <= MAX_CELLS {
                            self.address_cells = cells;
                        }
                    }
                }
                "#size-cells" => {
                    if let Some(cells) = read_be32(value, 0) {
                        if cells <= MAX_CELLS {
                            self.size_cells = cells;
                        }
                    }
                }
                _ => {}
            }
            return;
        }

        if name != "reg" {
            return;
        }
        // Only root-level `/memory` and `/memory@...` nodes are RAM.
        let memory_node = path == "/memory" || path.starts_with("/memory@");
        if !memory_node {
            return;
        }
        if !parse_regions(
            value,
            self.address_cells,
            self.size_cells,
            self.regions,
            &mut self.count,
        ) {
            self.malformed = true;
        }
    }

    fn end(&mut self, _path: &str) {}
}

struct ReservedRegions<'o> {
    regions: &'o mut [Region],
    count: usize,
    address_cells: u32,
    size_cells: u32,
    in_reserved: bool,
    depth: usize,
    malformed: bool,
}

impl<'a, 'o> Visitor<'a> for ReservedRegions<'o> {
    fn begin(&mut self, path: &str, _name: &str) {
        if path == "/reserved-memory" {
            self.in_reserved = true;
            self.depth = 0;
        } else if self.in_reserved && path.starts_with("/reserved-memory/") {
            self.depth += 1;
        }
    }

    fn property(&mut self, path: &str, name: &str, value: &'a [u8]) {
        if !self.in_reserved {
            return;
        }
        if path == "/reserved-memory" {
            match name {
                "#address-cells" => {
                    if let Some(cells) = read_be32(value, 0) {
                        if cells <= MAX_CELLS {
                            self.address_cells = cells;
                        }
                    }
                }
                "#size-cells" => {
                    if let Some(cells) = read_be32(value, 0) {
                        if cells <= MAX_CELLS {
                            self.size_cells = cells;
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        // Match the pre-reloc validator: every `reg` under /reserved-memory,
        // including nested children, is a carve-out.
        if name == "reg" {
            if !parse_regions(
                value,
                self.address_cells,
                self.size_cells,
                self.regions,
                &mut self.count,
            ) {
                self.malformed = true;
            }
        }
    }

    fn end(&mut self, path: &str) {
        if path == "/reserved-memory" {
            self.in_reserved = false;
        } else if self.in_reserved && path.starts_with("/reserved-memory/") {
            self.depth = self.depth.saturating_sub(1);
        }
    }
}

/// Counts `/cpus` children (`cpu` or `cpu@...`), bounded by the caller's slot
/// limit. Used to size AP bring-up from the live tree instead of assuming
/// `MAX_CPUS`; a count of 0 means the tree did not describe CPUs.
struct CpuCounter {
    count: usize,
    limit: usize,
}

impl<'a> Visitor<'a> for CpuCounter {
    fn begin(&mut self, path: &str, _name: &str) {
        // Only DIRECT `/cpus` children count. `path` is the full node path, so
        // require exactly one component below `/cpus`: a node named `cpu`
        // elsewhere in the tree must not inflate the slot count.
        let Some(rest) = path.strip_prefix("/cpus/") else {
            return;
        };
        if rest.contains('/') {
            return;
        }
        let is_cpu = rest == "cpu" || rest.starts_with("cpu@");
        if is_cpu && self.count < self.limit {
            self.count += 1;
        }
    }
    fn property(&mut self, _path: &str, _name: &str, _value: &'a [u8]) {}
    fn end(&mut self, _path: &str) {}
}

/// Reads `/psci/method`; unknown or absent values stay `None` so the caller
/// keeps the platform fallback rather than issuing a trapping conduit.
struct PsciFinder {
    method: Option<PsciMethod>,
}

impl<'a> Visitor<'a> for PsciFinder {
    fn begin(&mut self, _path: &str, _name: &str) {}
    fn property(&mut self, path: &str, name: &str, value: &'a [u8]) {
        if path != "/psci" || name != "method" || self.method.is_some() {
            return;
        }
        let text = value.split(|byte| *byte == 0).next().unwrap_or(value);
        self.method = match text {
            b"hvc" => Some(PsciMethod::Hvc),
            b"smc" => Some(PsciMethod::Smc),
            _ => None,
        };
    }
    fn end(&mut self, _path: &str) {}
}

#[derive(Clone, Copy, Default)]
struct CompatibleState {
    matched: bool,
    disabled: bool,
    reg: Option<Region>,
}

struct EnabledRegFinder<'a> {
    wanted: &'a str,
    found: Option<Region>,
    depth: usize,
    nodes: [CompatibleState; MAX_DEPTH],
}

impl<'a, 't> Visitor<'a> for EnabledRegFinder<'t> {
    fn begin(&mut self, _path: &str, _name: &str) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.nodes[self.depth] = CompatibleState::default();
        self.depth += 1;
    }

    fn property(&mut self, _path: &str, name: &str, value: &'a [u8]) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        let Some(state) = self.nodes.get_mut(index) else {
            return;
        };
        match name {
            "compatible" => {
                let mut rest = value;
                while let Some(text) = next_string(&mut rest) {
                    if text == self.wanted {
                        state.matched = true;
                    }
                }
            }
            "reg" if state.reg.is_none() => {
                state.reg = parse_reg_range(value);
            }
            "status" => {
                if let Some(text) = first_string(value) {
                    if text == "disabled" {
                        state.disabled = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        self.depth -= 1;
        let state = self.nodes[index];
        if self.found.is_none() && state.matched && !state.disabled {
            if let Some(reg) = state.reg {
                if !reg.is_empty() {
                    self.found = Some(reg);
                }
            }
        }
    }
}

/// Per-node state of the PCI host finder.
#[derive(Clone, Copy, Default)]
struct PciState {
    kind: Option<PciHostKind>,
    reg: Option<Region>,
    bus: Option<(u8, u8)>,
    disabled: bool,
}

struct PciFinder<'o> {
    out: &'o mut [PciHost],
    count: usize,
    depth: usize,
    nodes: [PciState; MAX_DEPTH],
}

impl<'a, 'o> Visitor<'a> for PciFinder<'o> {
    fn begin(&mut self, _path: &str, _name: &str) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.nodes[self.depth] = PciState::default();
        self.depth += 1;
    }

    fn property(&mut self, _path: &str, name: &str, value: &'a [u8]) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        let Some(state) = self.nodes.get_mut(index) else {
            return;
        };
        match name {
            "compatible" => {
                let mut rest = value;
                while let Some(text) = next_string(&mut rest) {
                    if text == "pci-host-ecam-generic" {
                        state.kind = Some(PciHostKind::Ecam);
                    } else if text == "brcm,bcm2712-pcie" {
                        state.kind = Some(PciHostKind::Bcm2712);
                    }
                }
            }
            "reg" if state.reg.is_none() => {
                state.reg = parse_reg_range(value);
            }
            "bus-range" => {
                if let (Some(start), Some(end)) = (read_be32(value, 0), read_be32(value, 4)) {
                    if start <= end && end <= 255 {
                        state.bus = Some((start as u8, end as u8));
                    }
                }
            }
            "status" => {
                if let Some(text) = first_string(value) {
                    if text == "disabled" {
                        state.disabled = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        self.depth -= 1;
        let state = self.nodes[index];
        let (Some(kind), Some(reg)) = (state.kind, state.reg) else {
            return;
        };
        if state.disabled || reg.is_empty() || self.count >= self.out.len() {
            return;
        }
        let (bus_start, mut bus_end) = state.bus.unwrap_or(match kind {
            PciHostKind::Ecam => (0, 255),
            PciHostKind::Bcm2712 => (1, 2),
        });
        if kind == PciHostKind::Ecam {
            // One megabyte of ECAM describes one bus.
            let buses = reg.size() / 0x10_0000;
            if buses == 0 {
                return;
            }
            let last = buses.saturating_sub(1).min(255) as u8;
            bus_end = bus_end.min(last.saturating_add(bus_start));
        }
        if bus_start > bus_end {
            return;
        }
        self.out[self.count] = PciHost::new(kind, reg.start, reg.size(), bus_start, bus_end);
        self.count += 1;
    }
}

/// Copies direct `/cpus/cpu*` `reg` values, treating a 64-bit `reg` as one
/// MPIDR cell pair and a 32-bit one as the low affinity word.
struct CpuMpidrs<'o> {
    out: &'o mut [u64],
    seen: usize,
}

impl<'a, 'o> Visitor<'a> for CpuMpidrs<'o> {
    fn begin(&mut self, path: &str, _name: &str) {
        let Some(rest) = path.strip_prefix("/cpus/") else {
            return;
        };
        if rest.contains('/') || (rest != "cpu" && !rest.starts_with("cpu@")) {
            return;
        }
        self.seen = self.seen.saturating_add(1);
    }
    fn property(&mut self, path: &str, name: &str, value: &'a [u8]) {
        if name != "reg" || self.seen == 0 {
            return;
        }
        let Some(rest) = path.strip_prefix("/cpus/") else {
            return;
        };
        if rest.contains('/') || (rest != "cpu" && !rest.starts_with("cpu@")) {
            return;
        }
        let index = self.seen - 1;
        if index >= self.out.len() {
            return;
        }
        let mpidr = if value.len() >= 8 {
            read_be64(value, 0)
        } else {
            read_be32(value, 0).map(u64::from)
        };
        if let Some(mpidr) = mpidr {
            self.out[index] = mpidr;
        }
    }
    fn end(&mut self, _path: &str) {}
}

/// Per-node state of the UART visitor.
///
/// One entry per nesting level: a matching node may have children (a debug
/// UART with a pinmux child, a GIC with a `v2m` frame), and a single shared
/// state would let the child's `begin` erase the parent's properties before the
/// parent's `end` ran. That was a real bug: the GIC on QEMU's own tree was
/// discarded because its `v2m` child reset the captured range.
#[derive(Clone, Copy, Default)]
struct UartState {
    base: Option<u64>,
    kind: Option<UartKind>,
    irq: Option<u32>,
    clock: Option<u32>,
    clock_phandle: Option<u32>,
    is_uart: bool,
    debug: bool,
    disabled: bool,
}

struct UartFinder {
    depth: usize,
    nodes: [UartState; MAX_DEPTH],
    best: Option<UartNode>,
}

impl Default for UartFinder {
    fn default() -> Self {
        UartFinder {
            depth: 0,
            nodes: [UartState::default(); MAX_DEPTH],
            best: None,
        }
    }
}

impl<'a> Visitor<'a> for UartFinder {
    fn begin(&mut self, _path: &str, name: &str) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.nodes[self.depth] = UartState {
            base: unit_address(name),
            ..UartState::default()
        };
        self.depth += 1;
    }

    fn property(&mut self, _path: &str, name: &str, value: &'a [u8]) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        let Some(state) = self.nodes.get_mut(index) else {
            return;
        };
        match name {
            "compatible" => {
                let mut rest = value;
                while let Some(text) = next_string(&mut rest) {
                    if text == "qcom,geni-debug-uart" {
                        state.is_uart = true;
                        state.debug = true;
                        state.kind = Some(UartKind::Geni);
                    } else if text == "qcom,geni-uart" {
                        state.is_uart = true;
                        state.kind = Some(UartKind::Geni);
                    } else if text == "arm,pl011" {
                        state.is_uart = true;
                        state.kind = Some(UartKind::Pl011);
                    }
                }
            }
            "reg" => {
                // QEMU's PL011 uses `reg = <base size>` with 2/2 cells; the
                // unit address is normally present too, but fall back to reg
                // when it is not.
                if state.base.is_none() {
                    state.base = parse_primary_address(value);
                }
            }
            "interrupts" => {
                if value.len() >= 12 && read_be32(value, 0) == Some(0) {
                    if let Some(number) = read_be32(value, 4) {
                        if number < 1024 {
                            state.irq = Some(32 + number);
                        }
                    }
                } else if value.len() >= 12 && read_be32(value, 0) == Some(1) {
                    if let Some(number) = read_be32(value, 4) {
                        if number < 16 {
                            state.irq = Some(16 + number);
                        }
                    }
                }
            }
            "clocks" if state.clock_phandle.is_none() => {
                if let Some(phandle) = read_be32(value, 0) {
                    state.clock_phandle = Some(phandle);
                }
            }
            // A UART names its reference clock in one of three ways: directly
            // as `clock-frequency`, as the first `assigned-clock-rates` cell, or
            // by phandle (`clocks = <&clk_uart>`, the Raspberry Pi spelling)
            // which `find_uart` resolves with `clock_rate_of` after the walk.
            "clock-frequency" | "assigned-clock-rates" if state.clock.is_none() => {
                if let Some(hz) = read_be32(value, 0) {
                    if (1_000_000..=1_000_000_000).contains(&hz) {
                        state.clock = Some(hz);
                    }
                }
            }
            "status" => {
                if let Some(text) = first_string(value) {
                    if text == "disabled" {
                        state.disabled = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        self.depth -= 1;
        let state = self.nodes[index];
        let (Some(base), Some(kind)) = (state.base, state.kind) else {
            return;
        };
        if !state.is_uart || state.disabled || base == 0 {
            return;
        }
        let candidate = UartNode {
            base,
            debug: state.debug,
            kind,
            irq: state.irq,
            clock: state.clock,
            clock_phandle: state.clock_phandle,
        };
        match self.best {
            None => self.best = Some(candidate),
            Some(existing) if !existing.debug && candidate.debug => self.best = Some(candidate),
            _ => {}
        }
    }
}

/// Per-node state of the clock-provider visitor (see [`UartState`]).
#[derive(Clone, Copy, Default)]
struct ClockState {
    phandle: Option<u32>,
    rate: Option<u32>,
}

struct ClockFinder {
    wanted: u32,
    depth: usize,
    nodes: [ClockState; MAX_DEPTH],
    rate: Option<u32>,
}

impl<'a> Visitor<'a> for ClockFinder {
    fn begin(&mut self, _path: &str, _name: &str) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.nodes[self.depth] = ClockState::default();
        self.depth += 1;
    }

    fn property(&mut self, _path: &str, name: &str, value: &'a [u8]) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        let Some(state) = self.nodes.get_mut(index) else {
            return;
        };
        match name {
            // Property order is not guaranteed: `clock-frequency` is often
            // written before `phandle`, so collect both and decide on `end`.
            "phandle" | "linux,phandle" => {
                state.phandle = read_be32(value, 0);
            }
            "clock-frequency" | "assigned-clock-rates" => {
                if let Some(hz) = read_be32(value, 0) {
                    if (1_000_000..=1_000_000_000).contains(&hz) {
                        state.rate = Some(hz);
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        self.depth -= 1;
        let state = self.nodes[index];
        if self.rate.is_none() && state.phandle == Some(self.wanted) {
            self.rate = state.rate;
        }
    }
}

/// Per-node state of the interrupt-controller visitor (see [`UartState`]).
#[derive(Clone, Copy, Default)]
struct GicState {
    kind: Option<GicKind>,
    distributor: Option<u64>,
    second: Option<u64>,
    second_size: Option<u64>,
    stride: Option<u64>,
}

struct GicFinder {
    depth: usize,
    nodes: [GicState; MAX_DEPTH],
    best: Option<GicInfo>,
}

impl Default for GicFinder {
    fn default() -> Self {
        GicFinder {
            depth: 0,
            nodes: [GicState::default(); MAX_DEPTH],
            best: None,
        }
    }
}

impl<'a> Visitor<'a> for GicFinder {
    fn begin(&mut self, _path: &str, _name: &str) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.nodes[self.depth] = GicState::default();
        self.depth += 1;
    }

    fn property(&mut self, _path: &str, name: &str, value: &'a [u8]) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        let Some(state) = self.nodes.get_mut(index) else {
            return;
        };
        match name {
            "compatible" => {
                let mut rest = value;
                while let Some(text) = next_string(&mut rest) {
                    if text == "arm,gic-v3" {
                        state.kind = Some(GicKind::V3);
                    } else if text == "arm,cortex-a15-gic" || text == "arm,gic-400" {
                        state.kind = Some(GicKind::V2);
                    }
                }
            }
            // Property order is NOT guaranteed: `dtc` emits alphabetical
            // order, which puts `reg` before `compatible`. Recording the range
            // unconditionally and validating at node end is what makes a
            // real-world tree parse.
            "reg" => {
                if let Some((distributor, _, second, second_size)) = parse_gic_reg(value) {
                    state.distributor = Some(distributor);
                    state.second = Some(second);
                    state.second_size = Some(second_size);
                }
            }
            "redistributor-stride" => {
                if let Some(cells) = read_be32(value, 0) {
                    if cells != 0 {
                        state.stride = Some(u64::from(cells));
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        self.depth -= 1;
        let state = self.nodes[index];
        let (Some(kind), Some(distributor), Some(second)) =
            (state.kind, state.distributor, state.second)
        else {
            return;
        };
        if distributor == 0 {
            return;
        }
        let info = GicInfo {
            kind,
            distributor,
            second,
            second_size: state.second_size.unwrap_or(0),
            stride: state.stride.unwrap_or(0x2_0000),
        };
        if self.best.is_none() {
            self.best = Some(info);
        }
    }
}

/// Per-node state of the driver-node visitor (see [`UartState`]).
#[derive(Clone, Copy, Default)]
struct DriverState {
    kind: Option<DriverKind>,
    range: Option<Region>,
    irq: Option<u32>,
    disabled: bool,
}

struct DriverFinder<'o> {
    out: &'o mut [DriverNode],
    count: usize,
    depth: usize,
    nodes: [DriverState; MAX_DEPTH],
}

impl<'a, 'o> Visitor<'a> for DriverFinder<'o> {
    fn begin(&mut self, _path: &str, _name: &str) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.nodes[self.depth] = DriverState::default();
        self.depth += 1;
    }

    fn property(&mut self, _path: &str, name: &str, value: &'a [u8]) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        let Some(state) = self.nodes.get_mut(index) else {
            return;
        };
        match name {
            "compatible" => {
                let mut rest = value;
                while let Some(text) = next_string(&mut rest) {
                    if text == "arm,pl011" {
                        state.kind = Some(DriverKind::Pl011);
                    } else if text == "qcom,geni-uart" || text == "qcom,geni-debug-uart" {
                        state.kind = Some(DriverKind::GeniUart);
                    }
                }
            }
            "reg" if state.range.is_none() => {
                state.range = parse_reg_range(value);
            }
            "interrupts" => {
                if value.len() >= 12 && read_be32(value, 0) == Some(0) {
                    if let Some(number) = read_be32(value, 4) {
                        if number < 1024 {
                            state.irq = Some(32 + number);
                        }
                    }
                } else if value.len() >= 12 && read_be32(value, 0) == Some(1) {
                    if let Some(number) = read_be32(value, 4) {
                        if number < 16 {
                            state.irq = Some(16 + number);
                        }
                    }
                }
            }
            "status" => {
                if let Some(text) = first_string(value) {
                    if text == "disabled" {
                        state.disabled = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        self.depth -= 1;
        let state = self.nodes[index];
        let (Some(kind), Some(mmio)) = (state.kind, state.range) else {
            return;
        };
        if state.disabled || self.count >= self.out.len() {
            return;
        }
        self.out[self.count] = DriverNode {
            kind,
            mmio,
            irq: state.irq,
        };
        self.count += 1;
    }
}

/// Per-node state of the timer visitor (see [`UartState`]).
#[derive(Clone, Copy, Default)]
struct TimerState {
    is_timer: bool,
    irq: Option<u32>,
}

struct TimerFinder {
    depth: usize,
    nodes: [TimerState; MAX_DEPTH],
    irq: Option<u32>,
}

impl Default for TimerFinder {
    fn default() -> Self {
        TimerFinder {
            depth: 0,
            nodes: [TimerState::default(); MAX_DEPTH],
            irq: None,
        }
    }
}

impl<'a> Visitor<'a> for TimerFinder {
    fn begin(&mut self, _path: &str, _name: &str) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.nodes[self.depth] = TimerState::default();
        self.depth += 1;
    }

    fn property(&mut self, _path: &str, name: &str, value: &'a [u8]) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        let Some(state) = self.nodes.get_mut(index) else {
            return;
        };
        match name {
            "compatible" => {
                let mut rest = value;
                while let Some(text) = next_string(&mut rest) {
                    if text == "arm,armv8-timer" {
                        state.is_timer = true;
                    }
                }
            }
            // Recorded regardless of order: `dtc` writes `interrupts` before
            // `compatible` on QEMU's own tree, and the match is decided at
            // node end instead of trusting property order.
            "interrupts" if state.irq.is_none() => {
                // The DT binding order is: secure physical, non-secure
                // physical, virtual, hypervisor. The second entry is the one
                // CNTP_* drives.
                if let Some(entry) = value.get(12..24) {
                    if read_be32(entry, 0) == Some(1) {
                        if let Some(ppi) = read_be32(entry, 4) {
                            if ppi < 16 {
                                state.irq = Some(16 + ppi);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {
        let Some(index) = self.depth.checked_sub(1) else {
            return;
        };
        self.depth -= 1;
        let state = self.nodes[index];
        if self.irq.is_none() && state.is_timer {
            self.irq = state.irq;
        }
    }
}

#[derive(Default)]
struct TimerRawFinder {
    current_timer: bool,
    have_interrupts: bool,
    facts: RawFacts,
}

impl<'a> Visitor<'a> for TimerRawFinder {
    fn begin(&mut self, _path: &str, _name: &str) {
        self.current_timer = false;
    }

    fn property(&mut self, _path: &str, name: &str, value: &'a [u8]) {
        match name {
            "compatible" => {
                let mut rest = value;
                while let Some(text) = next_string(&mut rest) {
                    if text == "arm,armv8-timer" {
                        self.current_timer = true;
                        self.facts.found = true;
                    }
                }
            }
            "interrupts" if self.current_timer && !self.have_interrupts => {
                self.have_interrupts = true;
                let cells = core::cmp::min(value.len() / 4, RAW_CELLS);
                self.facts.len = cells;
                let mut index = 0;
                while index < cells {
                    self.facts.cells[index] = read_be32(value, index * 4).unwrap_or(0);
                    index += 1;
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {}
}

#[derive(Default)]
struct GicRawFinder {
    current_gic: bool,
    have_reg: bool,
    facts: RawFacts,
}

impl<'a> Visitor<'a> for GicRawFinder {
    fn begin(&mut self, _path: &str, _name: &str) {
        self.current_gic = false;
    }

    fn property(&mut self, _path: &str, name: &str, value: &'a [u8]) {
        match name {
            "compatible" => {
                let mut rest = value;
                while let Some(text) = next_string(&mut rest) {
                    if text == "arm,gic-v3" {
                        self.current_gic = true;
                        self.facts.found = true;
                    }
                }
            }
            "reg" if self.current_gic && !self.have_reg => {
                self.have_reg = true;
                let cells = core::cmp::min(value.len() / 4, RAW_CELLS);
                self.facts.len = cells;
                let mut index = 0;
                while index < cells {
                    self.facts.cells[index] = read_be32(value, index * 4).unwrap_or(0);
                    index += 1;
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {}
}

struct AliasFinder<'a> {
    wanted: &'a str,
    value: Option<&'a str>,
}

impl<'a> Visitor<'a> for AliasFinder<'a> {
    fn begin(&mut self, _path: &str, _name: &str) {}

    fn property(&mut self, path: &str, name: &str, value: &'a [u8]) {
        if path == "/aliases" && name == self.wanted {
            if let Some(text) = first_string(value) {
                self.value = Some(text);
            }
        }
    }

    fn end(&mut self, _path: &str) {}
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

fn read_be32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_be64(data: &[u8], offset: usize) -> Option<u64> {
    let bytes = data.get(offset..offset.checked_add(8)?)?;
    let mut value = [0u8; 8];
    value.copy_from_slice(bytes);
    Some(u64::from_be_bytes(value))
}

fn first_string(value: &[u8]) -> Option<&str> {
    let end = value.iter().position(|&byte| byte == 0)?;
    str::from_utf8(&value[..end]).ok()
}

/// Iterates the NUL-terminated strings in a `compatible` property.
fn next_string<'a>(rest: &mut &'a [u8]) -> Option<&'a str> {
    if rest.is_empty() {
        return None;
    }
    let end = rest.iter().position(|&byte| byte == 0)?;
    let text = str::from_utf8(&rest[..end]).ok()?;
    *rest = &rest[end + 1..];
    Some(text)
}

fn unit_address(name: &str) -> Option<u64> {
    let (_, address) = name.rsplit_once('@')?;
    let digits = address.split(',').next()?;
    let value = u64::from_str_radix(digits, 16).ok()?;
    (value != 0).then_some(value)
}

/// Splits a `reg` property's first entry into a half-open region, accepting
/// the two common cell layouts (2/2 and 1/1) used by the platforms here.
fn parse_reg_range(value: &[u8]) -> Option<Region> {
    if value.len() >= 16 {
        let start = read_be64(value, 0)?;
        let size = read_be64(value, 8)?;
        if plausible_reg(start, size) {
            return Some(Region::new(start, start + size));
        }
    }
    if value.len() >= 8 {
        let start = read_be32(value, 0)? as u64;
        let size = read_be32(value, 4)? as u64;
        if plausible_reg(start, size) {
            return Some(Region::new(start, start + size));
        }
    }
    None
}

/// Parses the first `(address, size)` pair of a `reg` property, accepting the
/// two common cell layouts (2/2 and 1/1) and rejecting implausible sizes.
fn parse_primary_address(value: &[u8]) -> Option<u64> {
    if value.len() >= 16 {
        let address = read_be64(value, 0)?;
        let size = read_be64(value, 8)?;
        if plausible_reg(address, size) {
            return Some(address);
        }
    }
    if value.len() >= 8 {
        let address = read_be32(value, 0)? as u64;
        let size = read_be32(value, 4)? as u64;
        if plausible_reg(address, size) {
            return Some(address);
        }
    }
    None
}

/// Parses two `(address, size)` pairs for a GIC node. Qualcomm and QEMU both
/// use 2/2 cells; the 1/1 fallback keeps older trees readable.
fn parse_gic_reg(value: &[u8]) -> Option<(u64, u64, u64, u64)> {
    if value.len() >= 32 {
        let distributor = read_be64(value, 0)?;
        let distributor_size = read_be64(value, 8)?;
        let second = read_be64(value, 16)?;
        let second_size = read_be64(value, 24)?;
        if plausible_reg(distributor, distributor_size) && plausible_reg(second, second_size) {
            return Some((distributor, distributor_size, second, second_size));
        }
    }
    if value.len() >= 16 {
        let distributor = read_be32(value, 0)? as u64;
        let distributor_size = read_be32(value, 4)? as u64;
        let second = read_be32(value, 8)? as u64;
        let second_size = read_be32(value, 12)? as u64;
        if plausible_reg(distributor, distributor_size) && plausible_reg(second, second_size) {
            return Some((distributor, distributor_size, second, second_size));
        }
    }
    None
}

fn plausible_reg(address: u64, size: u64) -> bool {
    // Sizes are not necessarily powers of two (QEMU's GICv3 redistributor
    // region is 0x14_0000), so only reject the clearly impossible.
    address != 0
        && size != 0
        && size <= 0x1000_0000
        && address % 0x1000 == 0
        && address.checked_add(size).is_some()
}

fn read_cells(value: &[u8], cells: u32) -> Option<u64> {
    match cells {
        0 => Some(0),
        1 => read_be32(value, 0).map(u64::from),
        2 => read_be64(value, 0),
        _ => None,
    }
}

fn parse_regions(
    value: &[u8],
    address_cells: u32,
    size_cells: u32,
    out: &mut [Region],
    count: &mut usize,
) -> bool {
    if address_cells == 0 || address_cells > MAX_CELLS || size_cells > MAX_CELLS {
        return false;
    }
    let address_bytes = (address_cells as usize) * 4;
    let size_bytes = (size_cells as usize) * 4;
    let entry = address_bytes + size_bytes;
    if entry == 0 {
        return false;
    }

    let mut offset = 0usize;
    while offset + entry <= value.len() {
        let Some(start) = read_cells(&value[offset..], address_cells) else {
            return false;
        };
        let Some(size) = read_cells(&value[offset + address_bytes..], size_cells) else {
            return false;
        };
        if size != 0 {
            let Some(end) = start.checked_add(size) else {
                return false;
            };
            if *count >= out.len() {
                return false;
            }
            out[*count] = Region::new(start, end);
            *count += 1;
        }
        offset += entry;
    }
    true
}

// ---------------------------------------------------------------------------
// Boot-memory planner
// ---------------------------------------------------------------------------

/// Selects the largest page-aligned contiguous window of `memory` that does
/// not intersect any region in `excluded`.
///
/// `excluded` is sorted in place. The result must be at least `min_pages`
/// pages long; otherwise `None` is returned. This is intentionally
/// conservative: a range is only used when the live tree proves it is RAM and
/// no reservation, the kernel image, the DTB or the boot stack claims it.
/// Clips every region to `limit`, dropping empty results. Used to keep the
/// allocator inside the physical window the boot page tables actually map;
/// a larger bank outside that window is useless to this first PoC.
pub fn clip_regions(regions: &[Region], limit: Region, out: &mut [Region]) -> usize {
    let mut count = 0usize;
    for &region in regions {
        let clipped = region.intersect(limit);
        if !clipped.is_empty() && count < out.len() {
            out[count] = clipped;
            count += 1;
        }
    }
    count
}

pub fn select_allocator_window(
    memory: &[Region],
    excluded: &mut [Region],
    page_size: u64,
    min_pages: u64,
) -> Option<Region> {
    if page_size == 0 || min_pages == 0 {
        return None;
    }
    let min_size = page_size.checked_mul(min_pages)?;

    excluded.sort_unstable_by_key(|region| region.start);

    let mut best: Option<Region> = None;
    for &range in memory {
        if range.is_empty() {
            continue;
        }
        let start = align_up_u64(range.start, page_size);
        let end = align_down_u64(range.end, page_size);
        if end <= start {
            continue;
        }

        let mut cursor = start;
        for &block in excluded.iter() {
            if block.is_empty() || block.end <= cursor || block.start >= end {
                continue;
            }
            if block.start > cursor {
                consider(
                    &mut best,
                    align_region(Region::new(cursor, block.start.min(end)), page_size),
                    min_size,
                );
            }
            if block.end > cursor {
                cursor = block.end;
            }
            if cursor >= end {
                break;
            }
        }
        if cursor < end {
            consider(
                &mut best,
                align_region(Region::new(cursor, end), page_size),
                min_size,
            );
        }
    }
    best
}

fn align_region(region: Region, page_size: u64) -> Region {
    Region::new(
        align_up_u64(region.start, page_size),
        align_down_u64(region.end, page_size),
    )
}

fn consider(best: &mut Option<Region>, candidate: Region, min_size: u64) {
    if candidate.is_empty() || candidate.size() < min_size {
        return;
    }
    let replace = match *best {
        None => true,
        Some(current) => candidate.size() > current.size(),
    };
    if replace {
        *best = Some(candidate);
    }
}

const fn align_up_u64(value: u64, alignment: u64) -> u64 {
    if alignment == 0 {
        return value;
    }
    value.saturating_add(alignment - 1) & !(alignment - 1)
}

const fn align_down_u64(value: u64, alignment: u64) -> u64 {
    if alignment == 0 {
        return value;
    }
    value & !(alignment - 1)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        parse_regions, select_allocator_window, DriverKind, DriverNode, Fdt, GicKind, PsciMethod,
        Region, UartKind, FDT_MAGIC,
    };

    // --- a tiny DTB writer used only by the tests -------------------------

    struct Dtb {
        strings: Vec<u8>,
        string_offsets: Vec<(String, u32)>,
        structure: Vec<u8>,
        memreserve: Vec<(u64, u64)>,
    }

    impl Dtb {
        fn new() -> Self {
            Self {
                strings: Vec::new(),
                string_offsets: Vec::new(),
                structure: Vec::new(),
                memreserve: Vec::new(),
            }
        }

        fn string(&mut self, name: &str) -> u32 {
            if let Some((_, offset)) = self.string_offsets.iter().find(|(n, _)| n == name) {
                return *offset;
            }
            let offset = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            self.string_offsets.push((name.to_string(), offset));
            offset
        }

        fn begin(&mut self, name: &str) {
            self.push_u32(1);
            self.structure.extend_from_slice(name.as_bytes());
            self.structure.push(0);
            while self.structure.len() % 4 != 0 {
                self.structure.push(0);
            }
        }

        fn end(&mut self) {
            self.push_u32(2);
        }

        fn prop(&mut self, name: &str, value: &[u8]) {
            self.push_u32(3);
            self.push_u32(value.len() as u32);
            let offset = self.string(name);
            self.push_u32(offset);
            self.structure.extend_from_slice(value);
            while self.structure.len() % 4 != 0 {
                self.structure.push(0);
            }
        }

        fn prop_u32(&mut self, name: &str, value: u32) {
            self.prop(name, &value.to_be_bytes());
        }

        fn prop_str(&mut self, name: &str, value: &str) {
            let mut bytes = value.as_bytes().to_vec();
            bytes.push(0);
            self.prop(name, &bytes);
        }

        fn prop_reg2(&mut self, name: &str, address: u64, size: u64) {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&address.to_be_bytes());
            bytes.extend_from_slice(&size.to_be_bytes());
            self.prop(name, &bytes);
        }

        fn prop_reg1(&mut self, name: &str, address: u32, size: u32) {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&address.to_be_bytes());
            bytes.extend_from_slice(&size.to_be_bytes());
            self.prop(name, &bytes);
        }

        fn prop_two_reg2(&mut self, name: &str, a1: u64, s1: u64, a2: u64, s2: u64) {
            let mut bytes = Vec::new();
            for (address, size) in [(a1, s1), (a2, s2)] {
                bytes.extend_from_slice(&address.to_be_bytes());
                bytes.extend_from_slice(&size.to_be_bytes());
            }
            self.prop(name, &bytes);
        }

        fn prop_u32_array(&mut self, name: &str, values: &[u32]) {
            let mut bytes = Vec::new();
            for value in values {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            self.prop(name, &bytes);
        }

        fn push_u32(&mut self, value: u32) {
            self.structure.extend_from_slice(&value.to_be_bytes());
        }

        fn build(mut self) -> Vec<u8> {
            self.push_u32(9); // FDT_END

            let off_rsvmap = 40usize;
            let rsvmap_len = (self.memreserve.len() + 1) * 16;
            let off_struct = off_rsvmap + rsvmap_len;
            let off_strings = off_struct + self.structure.len();
            let totalsize = off_strings + self.strings.len();

            let mut blob = Vec::with_capacity(totalsize);
            blob.extend_from_slice(&FDT_MAGIC.to_be_bytes());
            blob.extend_from_slice(&(totalsize as u32).to_be_bytes());
            blob.extend_from_slice(&(off_struct as u32).to_be_bytes());
            blob.extend_from_slice(&(off_strings as u32).to_be_bytes());
            blob.extend_from_slice(&(off_rsvmap as u32).to_be_bytes());
            blob.extend_from_slice(&17u32.to_be_bytes()); // version
            blob.extend_from_slice(&16u32.to_be_bytes()); // last_comp_version
            blob.extend_from_slice(&0u32.to_be_bytes()); // boot_cpuid
            blob.extend_from_slice(&(self.strings.len() as u32).to_be_bytes());
            blob.extend_from_slice(&(self.structure.len() as u32).to_be_bytes());
            assert_eq!(blob.len(), 40);

            for (address, size) in &self.memreserve {
                blob.extend_from_slice(&address.to_be_bytes());
                blob.extend_from_slice(&size.to_be_bytes());
            }
            blob.extend_from_slice(&0u64.to_be_bytes());
            blob.extend_from_slice(&0u64.to_be_bytes());
            assert_eq!(blob.len(), off_rsvmap + rsvmap_len);

            blob.extend_from_slice(&self.structure);
            blob.extend_from_slice(&self.strings);
            assert_eq!(blob.len(), totalsize);
            blob
        }
    }

    fn sample_dtb() -> Dtb {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_str("model", "Xiaomi Redmi 10C");
        dtb.prop_str("compatible", "qcom,khaje-idp\0qcom,sm6225");
        dtb.prop_u32("#address-cells", 2);
        dtb.prop_u32("#size-cells", 2);

        dtb.begin("memory@40000000");
        dtb.prop_reg2("reg", 0x4000_0000, 0x3dd0_0000);
        dtb.end();

        dtb.begin("memory@80000000");
        dtb.prop_reg2("reg", 0x8000_0000, 0x4000_0000);
        dtb.end();

        dtb.begin("reserved-memory");
        dtb.prop_u32("#address-cells", 1);
        dtb.prop_u32("#size-cells", 1);
        dtb.begin("hyp@45700000");
        dtb.prop("reg", &[0x45, 0x70, 0x00, 0x00, 0x00, 0x60, 0x00, 0x00]);
        dtb.end();
        dtb.begin("splash@5c000000");
        dtb.prop("reg", &[0x5c, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x00, 0x00]);
        dtb.end();
        dtb.end();

        dtb.begin("intc@f200000");
        dtb.prop_str("compatible", "arm,gic-v3");
        dtb.prop_two_reg2("reg", 0x0f20_0000, 0x1_0000, 0x0f30_0000, 0x10_0000);
        dtb.prop_u32("redistributor-stride", 0x2_0000);
        dtb.end();

        dtb.begin("timer");
        dtb.prop_str("compatible", "arm,armv8-timer");
        dtb.prop_u32_array("interrupts", &[1, 1, 4, 1, 2, 4, 1, 3, 4, 1, 0, 4]);
        dtb.end();

        dtb.begin("chosen");
        dtb.prop_str("bootargs", "console=ttyMSM0,115200n8");
        dtb.prop_str(
            "stdout-path",
            "/soc/geniqup@4ac0000/serial@4a9000:115200",
        );
        dtb.end();

        dtb.begin("soc");
        dtb.prop_u32("#address-cells", 1);
        dtb.prop_u32("#size-cells", 1);
        dtb.begin("geniqup@4ac0000");
        dtb.prop_str("compatible", "qcom,geni-se-qup");
        dtb.begin("serial@4a9000");
        dtb.prop_str("compatible", "qcom,geni-debug-uart");
        dtb.prop_reg1("reg", 0x4a9000, 0x100);
        dtb.prop_str("status", "okay");
        dtb.end();
        dtb.begin("serial@4a8c000");
        dtb.prop_str("compatible", "qcom,geni-uart");
        dtb.prop_reg1("reg", 0x4a8c000, 0x100);
        dtb.end();
        dtb.end();
        dtb.end();

        dtb.end();
        dtb
    }

    // --- parser tests -----------------------------------------------------


    #[test]
    fn header_and_identity_are_parsed() {
        let blob = sample_dtb().build();
        let fdt = Fdt::new(&blob).expect("valid dtb");
        assert_eq!(fdt.total_size(), blob.len());
        assert_eq!(fdt.version(), 17);
        assert_eq!(fdt.root_model(), Some("Xiaomi Redmi 10C"));
        assert_eq!(fdt.root_compatible(), Some("qcom,khaje-idp"));
        assert_eq!(fdt.root_cells(), (2, 2));
    }

    #[test]
    fn read_cstr_stays_inside_the_strings_block() {
        let blob = sample_dtb().build();
        let fdt = Fdt::new(&blob).expect("valid dtb");
        let strings_end = fdt.off_strings + fdt.size_strings;
        assert!(fdt.read_cstr(fdt.off_strings, strings_end).is_some());

        // Put a non-NUL byte at the very end of the strings block. The next
        // byte belongs to the struct block and is zero, so an unbounded
        // reader would accept a name that crosses the boundary; the bounded
        // reader must reject it.
        let mut broken = blob.clone();
        broken[strings_end - 1] = b'X';
        let fdt = Fdt::new(&broken).expect("sizes still valid");
        assert_eq!(fdt.read_cstr(strings_end - 1, strings_end), None);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut blob = sample_dtb().build();
        blob[0] = 0;
        assert!(Fdt::new(&blob).is_none());
    }

    #[test]
    fn truncated_blob_is_rejected() {
        let blob = sample_dtb().build();
        assert!(Fdt::new(&blob[..blob.len() - 4]).is_none());
        assert!(Fdt::new(&blob[..20]).is_none());
    }

    #[test]
    fn out_of_range_struct_offset_is_rejected() {
        let mut blob = sample_dtb().build();
        let bad_offset = blob.len() as u32 + 4;
        blob[8..12].copy_from_slice(&bad_offset.to_be_bytes());
        assert!(Fdt::new(&blob).is_none());
    }

    #[test]
    fn memory_regions_use_two_cells() {
        let blob = sample_dtb().build();
        let fdt = Fdt::new(&blob).unwrap();
        let mut regions = [Region::default(); 4];
        let count = fdt.memory_regions(&mut regions);
        assert_eq!(count, 2);
        assert_eq!(regions[0], Region::new(0x4000_0000, 0x7dd0_0000));
        assert_eq!(regions[1], Region::new(0x8000_0000, 0xc000_0000));
    }

    #[test]
    fn reserved_regions_use_their_own_cells() {
        let blob = sample_dtb().build();
        let fdt = Fdt::new(&blob).unwrap();
        let mut regions = [Region::default(); 4];
        let count = fdt.reserved_regions(&mut regions).unwrap();
        assert_eq!(count, 2);
        assert_eq!(regions[0], Region::new(0x4570_0000, 0x45d0_0000));
        assert_eq!(regions[1], Region::new(0x5c00_0000, 0x5cf0_0000));
    }

    #[test]
    fn reserved_memory_overflow_fails_closed() {
        // `/reserved-memory` children use their own #address-cells/#size-cells;
        // an entry whose address + size wraps cannot be represented and must
        // fail the boot instead of being dropped from the exclusion list.
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_str("compatible", "test,board");
        dtb.begin("reserved-memory");
        dtb.prop_u32("#address-cells", 2);
        dtb.prop_u32("#size-cells", 2);
        dtb.begin("broken@f000000000000000");
        dtb.prop_reg2("reg", 0xF000_0000_0000_0000, 0x2000_0000_0000_0000);
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let fdt = Fdt::new(&blob).unwrap();
        let mut regions = [Region::default(); 4];
        assert_eq!(fdt.reserved_regions(&mut regions), None);
    }

    #[test]
    fn reserved_memory_out_of_space_fails_closed() {
        let blob = sample_dtb().build();
        let fdt = Fdt::new(&blob).unwrap();
        let mut regions = [Region::default(); 1];
        assert_eq!(fdt.reserved_regions(&mut regions), None);
    }

    #[test]
    fn chosen_and_uart_are_resolved() {
        let blob = sample_dtb().build();
        let fdt = Fdt::new(&blob).unwrap();
        assert_eq!(
            fdt.chosen_bootargs(),
            Some("console=ttyMSM0,115200n8")
        );
        let stdout = fdt.chosen_stdout().unwrap();
        assert_eq!(fdt.resolve_stdout_base(stdout), Some(0x4a9000));
        let uart = fdt.find_uart().unwrap();
        assert_eq!(uart.base, 0x4a9000);
        assert!(uart.debug);
        assert_eq!(uart.kind, UartKind::Geni);
    }

    #[test]
    fn driver_nodes_read_enabled_uart_ranges_and_irqs() {
        let blob = sample_dtb().build();
        let fdt = Fdt::new(&blob).unwrap();
        let mut nodes = [DriverNode {
            kind: DriverKind::Pl011,
            mmio: Region::default(),
            irq: None,
        }; 4];
        assert_eq!(fdt.driver_nodes(&mut nodes), 2);
        assert_eq!(nodes[0].kind, DriverKind::GeniUart);
        assert_eq!(nodes[0].mmio, Region::new(0x4a9000, 0x4a9100));
        assert_eq!(nodes[1].mmio, Region::new(0x4a8c000, 0x4a8c100));

        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.begin("serial@9000000");
        dtb.prop_str("compatible", "arm,pl011");
        dtb.prop_reg2("reg", 0x0900_0000, 0x1000);
        dtb.prop_u32_array("interrupts", &[0, 1, 4]); // SPI 1 -> INTID 33
        dtb.end();
        dtb.begin("serial@a000000");
        dtb.prop_str("compatible", "arm,pl011");
        dtb.prop_reg2("reg", 0x0a00_0000, 0x1000);
        dtb.prop_str("status", "disabled");
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let fdt = Fdt::new(&blob).unwrap();
        let mut nodes = [DriverNode {
            kind: DriverKind::Pl011,
            mmio: Region::default(),
            irq: None,
        }; 2];
        assert_eq!(fdt.driver_nodes(&mut nodes), 1);
        assert_eq!(nodes[0].kind, DriverKind::Pl011);
        assert_eq!(nodes[0].mmio, Region::new(0x0900_0000, 0x0900_1000));
        assert_eq!(nodes[0].irq, Some(33));
    }

    #[test]
    fn gic_and_timer_are_parsed() {
        let blob = sample_dtb().build();
        let fdt = Fdt::new(&blob).unwrap();
        let gic = fdt.gic().expect("gic node");
        assert_eq!(gic.kind, GicKind::V3);
        assert_eq!(gic.distributor, 0x0f20_0000);
        assert_eq!(gic.second, 0x0f30_0000);
        assert_eq!(gic.second_size, 0x10_0000);
        assert_eq!(gic.stride, 0x2_0000);
        assert_eq!(fdt.timer_irq(), Some(18));
    }

    #[test]
    fn pl011_is_recognised() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_u32("#address-cells", 2);
        dtb.prop_u32("#size-cells", 2);
        dtb.begin("pl011@9000000");
        dtb.prop_str("compatible", "arm,pl011");
        dtb.prop_two_reg2("reg", 0x0900_0000, 0x1000, 0, 0);
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let fdt = Fdt::new(&blob).unwrap();
        let uart = fdt.find_uart().unwrap();
        assert_eq!(uart.base, 0x0900_0000);
        assert_eq!(uart.kind, UartKind::Pl011);
    }

    #[test]
    fn timer_is_found_when_interrupts_precede_compatible() {
        // `dtc` writes QEMU's timer node as interrupts-first. Requiring
        // `compatible` to be seen before the cells made the timer PPI read as
        // absent on a real tree.
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_u32("#address-cells", 2);
        dtb.prop_u32("#size-cells", 2);
        dtb.begin("timer");
        dtb.prop_u32_array(
            "interrupts",
            &[1, 13, 0x304, 1, 14, 0x304, 1, 11, 0x304, 1, 10, 0x304],
        );
        dtb.prop_str("compatible", "arm,armv8-timer\0arm,armv7-timer");
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        // Non-secure physical timer is the second entry: PPI 14 -> INTID 30.
        assert_eq!(Fdt::new(&blob).unwrap().timer_irq(), Some(30));
    }

    #[test]
    fn a_matching_node_with_a_child_is_still_found() {
        // QEMU's GIC carries a `v2m` frame child, and real SoCs carry several
        // children under interrupt controllers. A visitor with one shared state
        // lost the parent's properties when the child's `begin` ran — the
        // controller was silently ignored on a real tree (live bug).
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_u32("#address-cells", 2);
        dtb.prop_u32("#size-cells", 2);
        dtb.begin("intc@8000000");
        dtb.prop_str("compatible", "arm,cortex-a15-gic");
        dtb.prop_two_reg2("reg", 0x0800_0000, 0x1_0000, 0x0801_0000, 0x1_0000);
        dtb.begin("v2m@8020000");
        dtb.prop_reg2("reg", 0x0802_0000, 0x1000);
        dtb.prop_str("compatible", "arm,gic-v2m-frame");
        dtb.end();
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let gic = Fdt::new(&blob).unwrap().gic().expect("gic found");
        assert_eq!(gic.distributor, 0x0800_0000);
        assert_eq!(gic.second, 0x0801_0000);
    }

    #[test]
    fn gic_is_found_when_reg_precedes_compatible() {
        // `dtc` sorts properties, so `reg` normally comes before `compatible`.
        // Parsing that order is the difference between taking interrupts from
        // the tree and silently ignoring the controller (a live bug: QEMU's
        // own tree is written this way).
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_u32("#address-cells", 2);
        dtb.prop_u32("#size-cells", 2);
        dtb.begin("intc@8000000");
        dtb.prop_two_reg2("reg", 0x0800_0000, 0x1_0000, 0x0801_0000, 0x1_0000);
        dtb.prop_str("compatible", "arm,cortex-a15-gic");
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let gic = Fdt::new(&blob).unwrap().gic().expect("gic found");
        assert_eq!(gic.kind, GicKind::V2);
        assert_eq!(gic.distributor, 0x0800_0000);
        assert_eq!(gic.second, 0x0801_0000);
        assert_eq!(gic.second_size, 0x1_0000);
    }

    #[test]
    fn uart_reference_clock_is_read_from_the_node() {
        // A UART's baud divisor depends on its reference clock: QEMU's PL011
        // runs at 24 MHz, a Pi 5 debug UART at 48 MHz. Reading it from the tree
        // is what removes the last hardcoded number from the console path.
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_u32("#address-cells", 1);
        dtb.prop_u32("#size-cells", 1);
        dtb.begin("serial@9000000");
        dtb.prop_str("compatible", "arm,pl011");
        dtb.prop_reg1("reg", 0x0900_0000, 0x1000);
        dtb.prop_u32("clock-frequency", 48_000_000);
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let uart = Fdt::new(&blob).unwrap().find_uart().unwrap();
        assert_eq!(uart.clock, Some(48_000_000));
    }

    #[test]
    fn assigned_clock_rates_is_accepted_as_the_reference_clock() {
        // `assigned-clock-rates` is the other spelling used on the node itself
        // (first cell wins). Boards that name a phandle instead are documented
        // as unsupported, not silently guessed.
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_u32("#address-cells", 1);
        dtb.prop_u32("#size-cells", 1);
        dtb.begin("serial@9000000");
        dtb.prop_str("compatible", "arm,pl011");
        dtb.prop_reg1("reg", 0x0900_0000, 0x1000);
        dtb.prop_u32_array("assigned-clock-rates", &[48_000_000, 24_000_000]);
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let uart = Fdt::new(&blob).unwrap().find_uart().unwrap();
        assert_eq!(uart.clock, Some(48_000_000));
    }

    #[test]
    fn the_reference_clock_is_resolved_through_a_phandle() {
        // The Raspberry Pi spelling: the UART points at a fixed-clock node.
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_u32("#address-cells", 1);
        dtb.prop_u32("#size-cells", 1);
        dtb.begin("clk-uart");
        dtb.prop_str("compatible", "fixed-clock");
        dtb.prop_u32("#clock-cells", 0);
        dtb.prop_u32("clock-frequency", 48_000_000);
        dtb.prop_u32("phandle", 0x8000);
        dtb.end();
        dtb.begin("serial@9000000");
        dtb.prop_str("compatible", "arm,pl011");
        dtb.prop_reg1("reg", 0x0900_0000, 0x1000);
        dtb.prop_u32_array("clocks", &[0x8000, 0]);
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let fdt = Fdt::new(&blob).unwrap();
        let uart = fdt.find_uart().unwrap();
        assert_eq!(uart.clock_phandle, Some(0x8000));
        assert_eq!(uart.clock, Some(48_000_000));
        // An unknown phandle stays unresolved instead of guessing.
        assert_eq!(fdt.clock_rate_of(0x8001), None);
        assert_eq!(fdt.clock_rate_of(0), None);
    }

    #[test]
    fn a_direct_clock_frequency_wins_over_a_phandle() {
        // Both present: the node's own value is the more specific statement.
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_u32("#address-cells", 1);
        dtb.prop_u32("#size-cells", 1);
        dtb.begin("clk-uart");
        dtb.prop_u32("clock-frequency", 24_000_000);
        dtb.prop_u32("phandle", 0x8000);
        dtb.end();
        dtb.begin("serial@9000000");
        dtb.prop_str("compatible", "arm,pl011");
        dtb.prop_reg1("reg", 0x0900_0000, 0x1000);
        dtb.prop_u32("clock-frequency", 48_000_000);
        dtb.prop_u32_array("clocks", &[0x8000, 0]);
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let uart = Fdt::new(&blob).unwrap().find_uart().unwrap();
        assert_eq!(uart.clock, Some(48_000_000));
    }

    #[test]
    fn a_node_without_a_reference_clock_leaves_it_unset() {
        // No `clock-frequency` means "the driver's default", not zero: a zero
        // divisor would program the UART into an unusable state.
        let blob = sample_dtb().build();
        let uart = Fdt::new(&blob).unwrap().find_uart().unwrap();
        assert_eq!(uart.clock, None);
    }

    #[test]
    fn an_implausible_reference_clock_is_ignored() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_u32("#address-cells", 1);
        dtb.prop_u32("#size-cells", 1);
        dtb.begin("serial@9000000");
        dtb.prop_str("compatible", "arm,pl011");
        dtb.prop_reg1("reg", 0x0900_0000, 0x1000);
        dtb.prop_u32("clock-frequency", 0);
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let uart = Fdt::new(&blob).unwrap().find_uart().unwrap();
        assert_eq!(uart.clock, None);
    }

    #[test]
    fn disabled_uart_is_skipped() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_u32("#address-cells", 1);
        dtb.prop_u32("#size-cells", 1);
        dtb.begin("serial@4a9000");
        dtb.prop_str("compatible", "qcom,geni-debug-uart");
        dtb.prop_str("status", "disabled");
        dtb.end();
        dtb.begin("serial@4a8c000");
        dtb.prop_str("compatible", "qcom,geni-uart");
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let fdt = Fdt::new(&blob).unwrap();
        let uart = fdt.find_uart().unwrap();
        assert_eq!(uart.base, 0x4a8c000);
        assert!(!uart.debug);
    }

    #[test]
    fn memreserve_map_is_parsed() {
        let mut dtb = sample_dtb();
        dtb.memreserve.push((0x4008_0000, 0x20_0000));
        dtb.memreserve.push((0x5c00_0000, 0x1000));
        let blob = dtb.build();
        let fdt = Fdt::new(&blob).unwrap();
        let mut regions = [Region::default(); 4];
        let count = fdt.memreserve_regions(&mut regions).unwrap();
        assert_eq!(count, 2);
        assert_eq!(regions[0], Region::new(0x4008_0000, 0x4028_0000));
        assert_eq!(regions[1], Region::new(0x5c00_0000, 0x5c00_1000));
    }

    #[test]
    fn cpu_count_reads_cpus_node() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_str("compatible", "test,board");
        dtb.begin("cpus");
        dtb.prop_u32("#address-cells", 1);
        dtb.prop_u32("#size-cells", 0);
        for index in 0..3 {
            dtb.begin(&format!("cpu@{index}"));
            dtb.prop_str("device_type", "cpu");
            dtb.end();
        }
        dtb.end();
        // A node named `cpu` outside /cpus must not be counted.
        dtb.begin("soc");
        dtb.begin("cpu@0");
        dtb.prop_str("device_type", "cpu");
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let fdt = Fdt::new(&blob).unwrap();
        assert_eq!(fdt.cpu_count(4), 3);
        // The slot limit wins when the tree describes more CPUs than we can
        // bring up.
        assert_eq!(fdt.cpu_count(2), 2);
    }

    #[test]
    fn cpu_count_absent_is_zero() {
        let blob = sample_dtb().build();
        let fdt = Fdt::new(&blob).unwrap();
        assert_eq!(fdt.cpu_count(4), 0);
    }

    #[test]
    fn memreserve_overflow_fails_closed() {
        let mut dtb = sample_dtb();
        // address + size wraps: the reservation cannot be represented, so the
        // parser must refuse instead of dropping it and handing the range to
        // the allocator.
        dtb.memreserve.push((u64::MAX - 0x10, 0x100));
        let blob = dtb.build();
        let fdt = Fdt::new(&blob).unwrap();
        let mut regions = [Region::default(); 4];
        assert_eq!(fdt.memreserve_regions(&mut regions), None);
    }

    #[test]
    fn memreserve_out_of_space_fails_closed() {
        let mut dtb = sample_dtb();
        dtb.memreserve.push((0x4008_0000, 0x20_0000));
        dtb.memreserve.push((0x5c00_0000, 0x1000));
        let blob = dtb.build();
        let fdt = Fdt::new(&blob).unwrap();
        let mut regions = [Region::default(); 1];
        assert_eq!(fdt.memreserve_regions(&mut regions), None);
    }

    #[test]
    fn alias_stdout_is_resolved() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.prop_u32("#address-cells", 1);
        dtb.prop_u32("#size-cells", 1);
        dtb.begin("aliases");
        dtb.prop_str("serial0", "/soc/serial@4a9000");
        dtb.end();
        dtb.begin("chosen");
        dtb.prop_str("stdout-path", "serial0:115200");
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let fdt = Fdt::new(&blob).unwrap();
        assert_eq!(
            fdt.resolve_stdout_base(fdt.chosen_stdout().unwrap()),
            Some(0x4a9000)
        );
    }

    // --- planner tests ----------------------------------------------------

    const PAGE: u64 = 4096;

    #[test]
    fn clip_regions_keeps_only_the_mapped_bank() {
        let memory = [
            Region::new(0x4000_0000, 0x7dd0_0000),
            Region::new(0x8000_0000, 0xc000_0000),
            Region::new(0x1_0000_0000, 0x1_4000_0000),
        ];
        let limit = Region::new(0x4000_0000, 0x8000_0000);
        let mut out = [Region::default(); 4];
        let count = super::clip_regions(&memory, limit, &mut out);
        assert_eq!(count, 1);
        assert_eq!(out[0], Region::new(0x4000_0000, 0x7dd0_0000));
    }

    #[test]
    fn planner_picks_the_largest_free_gap() {
        let memory = [Region::new(0x4000_0000, 0x8000_0000)];
        let mut excluded = [Region::new(0x4000_0000, 0x4020_0000)];
        let window = select_allocator_window(&memory, &mut excluded, PAGE, 16).unwrap();
        assert_eq!(window, Region::new(0x4020_0000, 0x8000_0000));
    }

    #[test]
    fn planner_handles_unsorted_and_overlapping_exclusions() {
        let memory = [Region::new(0x1000, 0x9000)];
        let mut excluded = [
            Region::new(0x7000, 0x8000),
            Region::new(0x2000, 0x4000),
            Region::new(0x3000, 0x5000),
        ];
        let window = select_allocator_window(&memory, &mut excluded, PAGE, 1).unwrap();
        // Free gaps: [0x1000, 0x2000), [0x5000, 0x7000) and [0x8000, 0x9000).
        // The middle gap is the largest, so it wins regardless of input order.
        assert_eq!(window, Region::new(0x5000, 0x7000));
    }

    #[test]
    fn planner_respects_minimum_size() {
        let memory = [Region::new(0x1000, 0x5000)];
        let mut excluded = [Region::new(0x2000, 0x4000)];
        assert_eq!(
            select_allocator_window(&memory, &mut excluded, PAGE, 2),
            None
        );
        assert_eq!(
            select_allocator_window(&memory, &mut excluded, PAGE, 1),
            Some(Region::new(0x1000, 0x2000))
        );
    }

    #[test]
    fn planner_aligns_to_pages() {
        let memory = [Region::new(0x1003, 0x5fff)];
        let mut excluded = [];
        let window = select_allocator_window(&memory, &mut excluded, PAGE, 1).unwrap();
        assert_eq!(window, Region::new(0x2000, 0x5000));
    }

    #[test]
    fn planner_aligns_after_unaligned_exclusions() {
        let memory = [Region::new(0x1000, 0x9000)];
        let mut excluded = [Region::new(0x2003, 0x4005)];
        let window = select_allocator_window(&memory, &mut excluded, PAGE, 1).unwrap();
        // Before: [0x1000, 0x2003) -> one aligned page.
        // After:  [0x4005, 0x9000) -> [0x5000, 0x9000), four pages, wins.
        assert_eq!(window, Region::new(0x5000, 0x9000));
    }

    #[test]
    fn planner_never_returns_a_reserved_range() {
        let memory = [Region::new(0x1_0000, 0x10_0000)];
        let mut excluded = [Region::new(0x4_0000, 0x5_0000)];
        let window = select_allocator_window(&memory, &mut excluded, PAGE, 1).unwrap();
        assert_eq!(window, Region::new(0x5_0000, 0x10_0000));
        assert!(!window.overlaps(Region::new(0x4_0000, 0x5_0000)));
    }

    #[test]
    fn planner_rejects_zero_arguments() {
        let memory = [Region::new(0x1000, 0x9000)];
        let mut excluded = [];
        assert_eq!(
            select_allocator_window(&memory, &mut excluded, 0, 1),
            None
        );
        assert_eq!(
            select_allocator_window(&memory, &mut excluded, PAGE, 0),
            None
        );
    }

    #[test]
    fn planner_ignores_reversed_regions() {
        let memory = [Region::new(0x9000, 0x1000)];
        let mut excluded = [Region::new(0x8000, 0x2000)];
        assert_eq!(
            select_allocator_window(&memory, &mut excluded, PAGE, 1),
            None
        );
    }

    #[test]
    fn psci_method_follows_the_tree() {
        for (text, expected) in [
            ("hvc", PsciMethod::Hvc),
            ("smc", PsciMethod::Smc),
        ] {
            let mut dtb = Dtb::new();
            dtb.begin("");
            dtb.begin("psci");
            dtb.prop_str("compatible", "arm,psci-1.0");
            dtb.prop_str("method", text);
            dtb.end();
            dtb.end();
            let blob = dtb.build();
            let tree = Fdt::new(&blob).expect("valid tree");
            assert_eq!(tree.psci_method(), Some(expected));
        }
    }

    #[test]
    fn psci_method_rejects_unknown_or_missing() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.begin("psci");
        dtb.prop_str("method", "hvc-and-a-half");
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let tree = Fdt::new(&blob).expect("valid tree");
        assert_eq!(tree.psci_method(), None);

        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.end();
        let blob = dtb.build();
        let tree = Fdt::new(&blob).expect("valid tree");
        assert_eq!(tree.psci_method(), None);
    }

    #[test]
    fn pci_host_reads_ecam_window_and_bus_range() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.begin("pci@40000000");
        dtb.prop_str("compatible", "pci-host-ecam-generic");
        dtb.prop_reg2("reg", 0x40_1000_0000, 0x1000_0000);
        dtb.prop_u32_array("bus-range", &[0, 255]);
        dtb.prop_str("status", "okay");
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let tree = Fdt::new(&blob).expect("valid tree");

        let mut hosts = [crate::pci::PciHost::new(
            crate::pci::PciHostKind::Bcm2712,
            0,
            0,
            0,
            0,
        ); 2];
        let count = tree.pci_hosts(&mut hosts);
        assert_eq!(count, 1);
        assert_eq!(hosts[0].kind, crate::pci::PciHostKind::Ecam);
        assert_eq!(hosts[0].base, 0x40_1000_0000);
        assert_eq!(hosts[0].bus_start, 0);
        assert_eq!(hosts[0].bus_end, 255);
    }

    #[test]
    fn pci_host_reads_bcm2712_and_skips_disabled_nodes() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.begin("axi");
        dtb.begin("pcie@1000100000");
        dtb.prop_str("compatible", "brcm,bcm2712-pcie");
        dtb.prop_reg2("reg", 0x10_0010_0000, 0x9310);
        dtb.prop_str("status", "disabled");
        dtb.end();
        dtb.begin("pcie@1000120000");
        dtb.prop_str("compatible", "brcm,bcm2712-pcie");
        dtb.prop_reg2("reg", 0x10_0012_0000, 0x9310);
        dtb.prop_str("status", "okay");
        dtb.end();
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let tree = Fdt::new(&blob).expect("valid tree");

        let mut hosts = [crate::pci::PciHost::new(
            crate::pci::PciHostKind::Ecam,
            0,
            0,
            0,
            0,
        ); 2];
        let count = tree.pci_hosts(&mut hosts);
        assert_eq!(count, 1);
        assert_eq!(hosts[0].kind, crate::pci::PciHostKind::Bcm2712);
        assert_eq!(hosts[0].base, 0x10_0012_0000);
        assert_eq!(hosts[0].bus_start, 1);
        assert_eq!(hosts[0].bus_end, 2);
    }

    #[test]
    fn ecam_bus_range_cannot_exceed_the_window() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.begin("pci@0");
        dtb.prop_str("compatible", "pci-host-ecam-generic");
        dtb.prop_reg2("reg", 0x40_1000_0000, 0x20_0000);
        dtb.prop_u32_array("bus-range", &[0, 255]);
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let tree = Fdt::new(&blob).expect("valid tree");

        let mut hosts = [crate::pci::PciHost::new(
            crate::pci::PciHostKind::Ecam,
            0,
            0,
            0,
            0,
        ); 1];
        assert_eq!(tree.pci_hosts(&mut hosts), 1);
        assert_eq!(hosts[0].bus_end, 1);
    }

    #[test]
    fn first_enabled_reg_skips_disabled_and_keeps_the_first_window() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.begin("mmc@fff000");
        dtb.prop_str("compatible", "brcm,bcm2712-sdhci");
        dtb.prop_reg2("reg", 0x10_00ff_f000, 0x260);
        dtb.end();
        dtb.begin("mmc@1100000");
        dtb.prop_str("compatible", "brcm,bcm2712-sdhci");
        dtb.prop_str("status", "disabled");
        dtb.prop_reg2("reg", 0x10_0110_0000, 0x260);
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let tree = Fdt::new(&blob).expect("valid tree");

        let reg = tree
            .first_enabled_reg("brcm,bcm2712-sdhci")
            .expect("enabled node");
        assert_eq!(reg, Region::new(0x10_00ff_f000, 0x10_00ff_f260));
    }

    #[test]
    fn first_enabled_reg_ignores_a_disabled_only_match() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.begin("mmc@fff000");
        dtb.prop_str("compatible", "brcm,bcm2712-sdhci");
        dtb.prop_str("status", "disabled");
        dtb.prop_reg2("reg", 0x10_00ff_f000, 0x260);
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let tree = Fdt::new(&blob).expect("valid tree");
        assert_eq!(tree.first_enabled_reg("brcm,bcm2712-sdhci"), None);
    }

    #[test]
    fn cpu_mpidrs_are_read_not_inferred_from_the_index() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.begin("cpus");
        dtb.prop_u32("#address-cells", 1);
        dtb.begin("cpu@0");
        dtb.prop_u32("reg", 0);
        dtb.end();
        dtb.begin("cpu@1");
        dtb.prop_u32("reg", 0x100);
        dtb.end();
        // A 64-bit `reg` is the MPIDR cell pair.
        let mp = 0x0000_0000_0000_0101_u64;
        dtb.begin("cpu@101");
        dtb.prop("reg", &mp.to_be_bytes());
        dtb.end();
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let tree = Fdt::new(&blob).expect("valid tree");

        let mut mpidrs = [u64::MAX; 4];
        let seen = tree.cpu_mpidrs(&mut mpidrs);
        assert_eq!(seen, 3);
        assert_eq!(mpidrs[..3], [0, 0x100, 0x101]);
        assert_eq!(mpidrs[3], u64::MAX);
    }

    #[test]
    fn cpu_mpidrs_clamp_without_overwriting_the_last_slot() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.begin("cpus");
        dtb.begin("cpu@0");
        dtb.prop_u32("reg", 0);
        dtb.end();
        dtb.begin("cpu@1");
        dtb.prop_u32("reg", 0x100);
        dtb.end();
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let tree = Fdt::new(&blob).expect("valid tree");

        let mut mpidrs = [u64::MAX; 1];
        assert_eq!(tree.cpu_mpidrs(&mut mpidrs), 1);
        assert_eq!(mpidrs, [0]);
    }

    #[test]
    fn cpu_mpidrs_ignore_nested_nodes_and_keep_the_sentinel() {
        let mut dtb = Dtb::new();
        dtb.begin("");
        dtb.begin("cpus");
        dtb.begin("cpu@0");
        dtb.prop_u32("reg", 0);
        dtb.begin("l2-cache");
        dtb.prop_u32("reg", 0x7fff);
        dtb.end();
        dtb.end();
        dtb.begin("cpu@1");
        dtb.end();
        dtb.end();
        dtb.end();
        let blob = dtb.build();
        let tree = Fdt::new(&blob).expect("valid tree");

        let mut mpidrs = [u64::MAX; 4];
        assert_eq!(tree.cpu_mpidrs(&mut mpidrs), 2);
        assert_eq!(mpidrs[0], 0);
        assert_eq!(mpidrs[1], u64::MAX);
    }

    #[test]
    fn region_parser_rejects_address_plus_size_overflow() {
        let mut value = [0u8; 16];
        value[..8].fill(0xFF);
        value[15] = 1;
        let mut regions = [Region::default(); 2];
        let mut count = 0;
        assert!(!parse_regions(&value, 2, 2, &mut regions, &mut count));
        assert_eq!(count, 0);
    }
}

