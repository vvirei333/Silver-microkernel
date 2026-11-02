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
}

/// GIC generation declared by the live tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GicKind {
    V2,
    V3,
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
        };
        if self.walk(&mut visitor).is_none() {
            return 0;
        }
        visitor.count
    }

    /// Copies `/reserved-memory` child regions into `out`.
    pub fn reserved_regions(&self, out: &mut [Region]) -> usize {
        let mut visitor = ReservedRegions {
            regions: out,
            count: 0,
            address_cells: 2,
            size_cells: 2,
            in_reserved: false,
            depth: 0,
        };
        if self.walk(&mut visitor).is_none() {
            return 0;
        }
        visitor.count
    }

    /// Copies the DTB header memory-reservation map into `out`. This is where
    /// the bootloader normally records the DTB itself and the ramdisk.
    pub fn memreserve_regions(&self, out: &mut [Region]) -> usize {
        let mut count = 0usize;
        let mut offset = self.off_rsvmap;
        while offset + 16 <= self.totalsize {
            let Some(address) = read_be64(self.data, offset) else {
                break;
            };
            let Some(size) = read_be64(self.data, offset + 8) else {
                break;
            };
            if address == 0 && size == 0 {
                break;
            }
            if size != 0
                && address.checked_add(size).is_some()
                && count < out.len()
            {
                out[count] = Region::new(address, address + size);
                count += 1;
            }
            offset += 16;
        }
        count
    }

    /// Finds the best GENI UART node: a `qcom,geni-debug-uart` wins over a
    /// plain `qcom,geni-uart`; `status = "disabled"` nodes are ignored.
    pub fn find_uart(&self) -> Option<UartNode> {
        let mut visitor = UartFinder::default();
        self.walk(&mut visitor)?;
        visitor.best
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
                    let name = self.read_cstr(offset)?;
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
                    let name = self.read_cstr(name_start)?;
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

    fn read_cstr(&self, offset: usize) -> Option<&'a str> {
        if offset >= self.totalsize {
            return None;
        }
        let remaining = &self.data[offset..];
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
        parse_regions(value, self.address_cells, self.size_cells, self.regions, &mut self.count);
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
        // Only direct children carry the actual carve-out ranges.
        if name == "reg" && self.depth == 1 {
            parse_regions(value, self.address_cells, self.size_cells, self.regions, &mut self.count);
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

#[derive(Default)]
struct UartFinder {
    current_base: Option<u64>,
    current_kind: Option<UartKind>,
    current_irq: Option<u32>,
    current_is_uart: bool,
    current_debug: bool,
    current_disabled: bool,
    best: Option<UartNode>,
}

impl<'a> Visitor<'a> for UartFinder {
    fn begin(&mut self, _path: &str, name: &str) {
        self.current_base = unit_address(name);
        self.current_kind = None;
        self.current_irq = None;
        self.current_is_uart = false;
        self.current_debug = false;
        self.current_disabled = false;
    }

    fn property(&mut self, _path: &str, name: &str, value: &'a [u8]) {
        match name {
            "compatible" => {
                let mut rest = value;
                while let Some(text) = next_string(&mut rest) {
                    if text == "qcom,geni-debug-uart" {
                        self.current_is_uart = true;
                        self.current_debug = true;
                        self.current_kind = Some(UartKind::Geni);
                    } else if text == "qcom,geni-uart" {
                        self.current_is_uart = true;
                        self.current_kind = Some(UartKind::Geni);
                    } else if text == "arm,pl011" {
                        self.current_is_uart = true;
                        self.current_kind = Some(UartKind::Pl011);
                    }
                }
            }
            "reg" => {
                // QEMU's PL011 uses `reg = <base size>` with 2/2 cells; the
                // unit address is normally present too, but fall back to reg
                // when it is not.
                if self.current_base.is_none() {
                    self.current_base = parse_primary_address(value);
                }
            }
            "interrupts" => {
                if value.len() >= 12 && read_be32(value, 0) == Some(0) {
                    if let Some(number) = read_be32(value, 4) {
                        if number < 1024 {
                            self.current_irq = Some(32 + number);
                        }
                    }
                } else if value.len() >= 12 && read_be32(value, 0) == Some(1) {
                    if let Some(number) = read_be32(value, 4) {
                        if number < 16 {
                            self.current_irq = Some(16 + number);
                        }
                    }
                }
            }
            "status" => {
                if let Some(text) = first_string(value) {
                    if text == "disabled" {
                        self.current_disabled = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {
        let Some(base) = self.current_base.take() else {
            return;
        };
        let Some(kind) = self.current_kind.take() else {
            return;
        };
        if !self.current_is_uart || self.current_disabled || base == 0 {
            return;
        }
        let candidate = UartNode {
            base,
            debug: self.current_debug,
            kind,
            irq: self.current_irq.take(),
        };
        match self.best {
            None => self.best = Some(candidate),
            Some(existing) if !existing.debug && candidate.debug => self.best = Some(candidate),
            _ => {}
        }
    }
}

#[derive(Default)]
struct GicFinder {
    current_kind: Option<GicKind>,
    distributor: Option<u64>,
    second: Option<u64>,
    second_size: Option<u64>,
    stride: Option<u64>,
    best: Option<GicInfo>,
}

impl<'a> Visitor<'a> for GicFinder {
    fn begin(&mut self, _path: &str, _name: &str) {
        self.current_kind = None;
        self.distributor = None;
        self.second = None;
        self.second_size = None;
        self.stride = None;
    }

    fn property(&mut self, _path: &str, name: &str, value: &'a [u8]) {
        match name {
            "compatible" => {
                let mut rest = value;
                while let Some(text) = next_string(&mut rest) {
                    if text == "arm,gic-v3" {
                        self.current_kind = Some(GicKind::V3);
                    } else if text == "arm,cortex-a15-gic" || text == "arm,gic-400" {
                        self.current_kind = Some(GicKind::V2);
                    }
                }
            }
            "reg" => {
                if self.current_kind.is_some() {
                    if let Some((distributor, _, second, second_size)) = parse_gic_reg(value) {
                        self.distributor = Some(distributor);
                        self.second = Some(second);
                        self.second_size = Some(second_size);
                    }
                }
            }
            "redistributor-stride" => {
                if let Some(cells) = read_be32(value, 0) {
                    if cells != 0 {
                        self.stride = Some(u64::from(cells));
                    }
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, _path: &str) {
        let (Some(kind), Some(distributor), Some(second)) =
            (self.current_kind, self.distributor, self.second)
        else {
            return;
        };
        let info = GicInfo {
            kind,
            distributor,
            second,
            second_size: self.second_size.unwrap_or(0),
            stride: self.stride.unwrap_or(0x2_0000),
        };
        if self.best.is_none() {
            self.best = Some(info);
        }
    }
}

#[derive(Default)]
struct TimerFinder {
    current_timer: bool,
    irq: Option<u32>,
}

impl<'a> Visitor<'a> for TimerFinder {
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
                    }
                }
            }
            "interrupts" => {
                if self.current_timer && self.irq.is_none() {
                    // The DT binding order is: secure physical, non-secure
                    // physical, virtual, hypervisor. The second entry is the
                    // one CNTP_* drives.
                    let second = value.get(12..24);
                    if let Some(entry) = second {
                        if read_be32(entry, 0) == Some(1) {
                            if let Some(ppi) = read_be32(entry, 4) {
                                if ppi < 16 {
                                    self.irq = Some(16 + ppi);
                                }
                            }
                        }
                    }
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
) {
    if address_cells == 0 || address_cells > MAX_CELLS || size_cells > MAX_CELLS {
        return;
    }
    let address_bytes = (address_cells as usize) * 4;
    let size_bytes = (size_cells as usize) * 4;
    let entry = address_bytes + size_bytes;
    if entry == 0 {
        return;
    }

    let mut offset = 0usize;
    while offset + entry <= value.len() {
        let Some(start) = read_cells(&value[offset..], address_cells) else {
            return;
        };
        let Some(size) = read_cells(&value[offset + address_bytes..], size_cells) else {
            return;
        };
        if size != 0 && start.checked_add(size).is_some() && *count < out.len() {
            out[*count] = Region::new(start, start + size);
            *count += 1;
        }
        offset += entry;
    }
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
    use super::{select_allocator_window, Fdt, GicKind, Region, UartKind, FDT_MAGIC};

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
        let count = fdt.reserved_regions(&mut regions);
        assert_eq!(count, 2);
        assert_eq!(regions[0], Region::new(0x4570_0000, 0x45d0_0000));
        assert_eq!(regions[1], Region::new(0x5c00_0000, 0x5cf0_0000));
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
        let count = fdt.memreserve_regions(&mut regions);
        assert_eq!(count, 2);
        assert_eq!(regions[0], Region::new(0x4008_0000, 0x4028_0000));
        assert_eq!(regions[1], Region::new(0x5c00_0000, 0x5c00_1000));
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
}

