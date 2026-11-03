//! Pre-relocation FDT validation for the fog self-relocation.
//!
//! This function runs from the boot stub **before** the image is copied: the
//! MMU is off, the image may still be at the address ABL chose, and there is
//! no allocator or Rust runtime. It therefore:
//!
//! * takes only the DTB pointer and the source/destination spans,
//! * reads the flattened device tree with raw big-endian loads,
//! * uses no statics, no allocation, no formatting and no panics,
//! * returns `1` only when the destination span is provably inside an
//!   available `/memory` region and outside every memreserve,
//!   `/reserved-memory` descendant, the DTB itself, the running source image
//!   and a `/chosen` initrd range.
//!
//! Fail-closed rules added for Package A2:
//! * `/chosen/linux,initrd-start` and `linux,initrd-end` must both exist, both
//!   parse as one 32-bit or one 64-bit cell, and form a non-wrapping range
//!   that does not overlap the destination;
//! * any non-empty `ranges` under `/reserved-memory` is fatal, even if no
//!   child has a `reg`;
//! * every descendant of `/reserved-memory` with `reg` is checked;
//! * `size`, `alloc-ranges` or `memory-region` without a checked `reg` is
//!   fatal; `memory-region` is always fatal because phandle resolution is not
//!   implemented;
//! * `/memory` counts only when its `status` is absent, `okay` or `ok`.
//!
//! This validator is a **necessary but not sufficient** ownership proof. It
//! only sees the live FDT. The fixed link address `0x40080000` is safe only
//! under a platform boot contract that guarantees ABL/firmware/DMA do not use
//! that slot and that the DTB is complete.
//!
//! Cell layouts follow the ownership nodes, not the whole tree:
//!
//! * the root node's `#address-cells`/`#size-cells` define the format of the
//!   `reg` properties of its immediate children, in particular `/memory`;
//! * a `/reserved-memory` node's `#address-cells`/`#size-cells` define the
//!   format of the `reg` properties of its own children;
//! * `reg` on any other node (`/cpus`, `/soc`, device nodes) is not
//!   interpreted at all.
//!
//! Both 1 and 2 cells are supported for each component. Any other value on an
//! ownership node fails closed. When an ownership node omits one of the
//! properties, the pre-existing 2/2 default is used. This is what lets a
//! normal `/cpus` 1/0 node coexist with the 2/2 memory layout the QEMU and
//! khaje trees use.

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

const FDT_HEADER_SIZE: usize = 40;
const MAX_DEPTH: usize = 32;
const MAX_MEMRESERVE: usize = 64;

/// The boot page tables map exactly the first DRAM gigabyte. The DTB pointer
/// and both spans are checked against it before any read, so a wild `x0`
/// cannot turn into a wild memory access while the MMU is off.
const DRAM_START: u64 = 0x4000_0000;
const DRAM_END: u64 = 0x8000_0000;

#[inline(always)]
unsafe fn read_u8(base: usize, offset: usize) -> u8 {
    core::ptr::read_volatile((base + offset) as *const u8)
}

#[inline(always)]
unsafe fn read_be32(base: usize, offset: usize) -> u32 {
    ((read_u8(base, offset) as u32) << 24)
        | ((read_u8(base, offset + 1) as u32) << 16)
        | ((read_u8(base, offset + 2) as u32) << 8)
        | (read_u8(base, offset + 3) as u32)
}

#[inline(always)]
unsafe fn read_be64(base: usize, offset: usize) -> u64 {
    ((read_be32(base, offset) as u64) << 32) | read_be32(base, offset + 4) as u64
}

#[inline(always)]
fn align4(value: usize) -> usize {
    (value + 3) & !3
}

#[inline(always)]
fn overlaps(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

unsafe fn name_eq(base: usize, start: usize, len: usize, expected: &[u8]) -> bool {
    if len != expected.len() {
        return false;
    }
    let mut index = 0;
    while index < len {
        let Some(&byte) = expected.get(index) else {
            return false;
        };
        if read_u8(base, start + index) != byte {
            return false;
        }
        index += 1;
    }
    true
}

unsafe fn node_is_memory(base: usize, start: usize, len: usize) -> bool {
    // "memory" or "memory@<unit-address>".
    if len < 6 {
        return false;
    }
    if !name_eq(base, start, 6, b"memory") {
        return false;
    }
    len == 6 || read_u8(base, start + 6) == b'@'
}

unsafe fn node_is_reserved_memory(base: usize, start: usize, len: usize) -> bool {
    // "reserved-memory" or "reserved-memory@<unit-address>".
    if len < 15 {
        return false;
    }
    if !name_eq(base, start, 15, b"reserved-memory") {
        return false;
    }
    len == 15 || read_u8(base, start + 15) == b'@'
}

unsafe fn prop_is(base: usize, start: usize, len: usize, expected: &[u8]) -> bool {
    name_eq(base, start, len, expected)
}

unsafe fn value_is_memory(base: usize, start: usize, len: usize) -> bool {
    len == 7 && name_eq(base, start, 7, b"memory\0")
}

/// Reads one cell value: one 32-bit cell or two 32-bit cells forming a
/// 64-bit big-endian value. The caller has already validated the width.
#[inline(always)]
unsafe fn read_cells(base: usize, offset: usize, cells: u32) -> u64 {
    match cells {
        1 => u64::from(read_be32(base, offset)),
        2 => read_be64(base, offset),
        _ => 0,
    }
}

/// Parses a `#address-cells`/`#size-cells` value on an ownership node.
/// Only 1 and 2 are understood; everything else fails closed.
unsafe fn parse_cells(base: usize, value: usize, len: usize) -> Option<u32> {
    if len != 4 {
        return None;
    }
    match read_be32(base, value) {
        1 => Some(1),
        2 => Some(2),
        _ => None,
    }
}

/// Scans one `reg` value with the cell layout of its parent node.
///
/// Each entry is `address_cells + size_cells` 32-bit cells. Returns
/// `(valid, destination_inside, destination_overlaps)`.
unsafe fn scan_reg(
    base: usize,
    value: usize,
    len: usize,
    address_cells: u32,
    size_cells: u32,
    dst_start: u64,
    dst_end: u64,
) -> (bool, bool, bool) {
    if len == 0 {
        return (true, false, false);
    }
    if (address_cells != 1 && address_cells != 2)
        || (size_cells != 1 && size_cells != 2)
    {
        return (false, false, false);
    }
    let entry = ((address_cells + size_cells) as usize) * 4;
    if len % entry != 0 {
        return (false, false, false);
    }
    let mut inside = false;
    let mut overlap = false;
    let mut offset = 0;
    while offset < len {
        let address = read_cells(base, value + offset, address_cells);
        let size = read_cells(base, value + offset + address_cells as usize * 4, size_cells);
        if size != 0 {
            let end = match address.checked_add(size) {
                Some(end) => end,
                None => return (false, false, false),
            };
            if dst_start >= address && dst_end <= end {
                inside = true;
            }
            if overlaps(dst_start, dst_end, address, end) {
                overlap = true;
            }
        }
        offset += entry;
    }
    (true, inside, overlap)
}

#[inline(always)]
fn in_dram_span(start: u64, end: u64) -> bool {
    start >= DRAM_START && end <= DRAM_END && start < end
}

/// Returns `1` when the destination span is safe to copy to, `0` otherwise.
///
/// Safety: `dtb` must point to a readable live FDT. The function only reads
/// memory inside the DTB's `totalsize` plus the caller-supplied spans.
#[inline(never)]
pub unsafe fn validate_relocation(
    dtb: *const u8,
    dst_start: u64,
    dst_end: u64,
    src_start: u64,
    src_end: u64,
) -> u64 {
    let base = dtb as usize;
    if base == 0 {
        return 0;
    }
    let base64 = base as u64;
    let header_end = match base64.checked_add(FDT_HEADER_SIZE as u64) {
        Some(end) => end,
        None => return 0,
    };
    if !in_dram_span(base64, header_end)
        || !in_dram_span(dst_start, dst_end)
        || !in_dram_span(src_start, src_end)
    {
        return 0;
    }
    validate_relocation_inner(base, dst_start, dst_end, src_start, src_end, DRAM_END)
}

unsafe fn node_is_chosen(base: usize, start: usize, len: usize) -> bool {
    len == 6 && name_eq(base, start, 6, b"chosen")
}

unsafe fn status_is_okay(base: usize, start: usize, len: usize) -> bool {
    (len == 5 && name_eq(base, start, 5, b"okay\0"))
        || (len == 3 && name_eq(base, start, 3, b"ok\0"))
}

unsafe fn name_contains(base: usize, start: usize, len: usize, needle: &[u8]) -> bool {
    if needle.is_empty() || len < needle.len() {
        return false;
    }
    let mut index = 0;
    while index + needle.len() <= len {
        if name_eq(base, start + index, needle.len(), needle) {
            return true;
        }
        index += 1;
    }
    false
}

/// Parses `linux,initrd-start`/`end`: a single 32-bit cell or a 64-bit cell.
/// Any other length is an unknown form and must fail closed.
unsafe fn parse_initrd_value(base: usize, value: usize, len: usize) -> Option<u64> {
    match len {
        4 => Some(u64::from(read_be32(base, value))),
        8 => Some(read_be64(base, value)),
        _ => None,
    }
}

unsafe fn validate_relocation_inner(
    base: usize,
    dst_start: u64,
    dst_end: u64,
    src_start: u64,
    src_end: u64,
    limit: u64,
) -> u64 {
    if dst_start >= dst_end || src_start >= src_end {
        return 0;
    }

    let totalsize = read_be32(base, 4) as usize;
    if read_be32(base, 0) != FDT_MAGIC || totalsize < FDT_HEADER_SIZE {
        return 0;
    }
    let base_end = match base.checked_add(totalsize) {
        Some(end) if end as u64 <= limit => end,
        _ => return 0,
    };
    let dtb_start = base as u64;
    let dtb_end = base_end as u64;

    // The destination must not clobber the running image or the DTB.
    if overlaps(dst_start, dst_end, src_start, src_end)
        || overlaps(dst_start, dst_end, dtb_start, dtb_end)
    {
        return 0;
    }

    let off_struct = read_be32(base, 8) as usize;
    let off_strings = read_be32(base, 12) as usize;
    let off_rsv = read_be32(base, 16) as usize;
    let size_strings = read_be32(base, 32) as usize;
    let size_struct = read_be32(base, 36) as usize;

    let struct_end = match off_struct.checked_add(size_struct) {
        Some(end) if end <= totalsize => end,
        _ => return 0,
    };
    let strings_end = match off_strings.checked_add(size_strings) {
        Some(end) if end <= totalsize => end,
        _ => return 0,
    };
    if off_rsv >= totalsize {
        return 0;
    }

    // Memory reservation map: (address, size) big-endian pairs, 0/0 ends it.
    let mut rsv = off_rsv;
    let mut rsv_count = 0;
    loop {
        if rsv_count >= MAX_MEMRESERVE || rsv.checked_add(16).map_or(true, |end| end > totalsize) {
            return 0;
        }
        let address = read_be64(base, rsv);
        let size = read_be64(base, rsv + 8);
        if address == 0 && size == 0 {
            break;
        }
        if size != 0 {
            let end = match address.checked_add(size) {
                Some(end) => end,
                None => return 0,
            };
            if overlaps(dst_start, dst_end, address, end) {
                return 0;
            }
        }
        rsv += 16;
        rsv_count += 1;
    }

    let mut pos = off_struct;
    let mut depth: usize = 0;
    let mut found_memory = false;
    let mut destination_inside = false;
    let mut source_inside = false;
    let mut reserved_depth: usize = usize::MAX;
    let mut chosen_depth: usize = usize::MAX;
    let mut reserved_has_ranges = false;
    let mut initrd_start: Option<u64> = None;
    let mut initrd_end: Option<u64> = None;
    // Cell layouts of the two ownership containers. Defaults match the
    // synthetic and khaje trees; a node that declares 1/1 switches its own
    // children to the compact layout.
    let mut root_address_cells: u32 = 2;
    let mut root_size_cells: u32 = 2;
    let mut reserved_address_cells: u32 = 2;
    let mut reserved_size_cells: u32 = 2;
    // Per-open-node flags, one bit per depth. Ten `u64`s instead of a large
    // array: an array initializer can lower to `memset`, and compiler memset
    // uses NEON on aarch64, which is not enabled before the EL2 hand-over.
    // That would turn FDT validation into an undefined-instruction fault.
    let mut name_memory: u64 = 0;
    let mut device_type_memory: u64 = 0;
    let mut status_bad: u64 = 0;
    let mut has_reg: u64 = 0;
    let mut reg_invalid: u64 = 0;
    let mut reg_dst_inside: u64 = 0;
    let mut reg_src_inside: u64 = 0;
    let mut reg_overlaps_dst: u64 = 0;
    let mut has_size: u64 = 0;
    let mut has_alloc_ranges: u64 = 0;
    let mut has_memory_region: u64 = 0;

    loop {
        if pos.checked_add(4).map_or(true, |end| end > struct_end) {
            return 0;
        }
        let token = read_be32(base, pos);
        pos += 4;

        match token {
            FDT_BEGIN_NODE => {
                if depth >= MAX_DEPTH {
                    return 0;
                }
                let name_start = pos;
                while pos < struct_end && read_u8(base, pos) != 0 {
                    pos += 1;
                }
                if pos >= struct_end {
                    return 0;
                }
                let name_len = pos - name_start;
                pos = align4(pos + 1);
                if pos > struct_end {
                    return 0;
                }
                depth += 1;
                let bit = 1_u64 << depth;
                name_memory &= !bit;
                device_type_memory &= !bit;
                status_bad &= !bit;
                has_reg &= !bit;
                reg_invalid &= !bit;
                reg_dst_inside &= !bit;
                reg_src_inside &= !bit;
                reg_overlaps_dst &= !bit;
                has_size &= !bit;
                has_alloc_ranges &= !bit;
                has_memory_region &= !bit;
                if node_is_memory(base, name_start, name_len) {
                    name_memory |= bit;
                }
                if node_is_reserved_memory(base, name_start, name_len) {
                    // A second/nested reserved-memory container would defeat
                    // the single-depth scan below. It is not a form this
                    // validator understands, so fail closed.
                    if reserved_depth != usize::MAX {
                        return 0;
                    }
                    reserved_depth = depth;
                }
                if node_is_chosen(base, name_start, name_len) {
                    chosen_depth = depth;
                }
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return 0;
                }
                let bit = 1_u64 << depth;
                // Only an immediate root child can be an ownership memory
                // node: the root's cell layout defines its reg. A deeper
                // "memory" node is not part of the proof and cannot make the
                // destination safe.
                let is_memory = depth == 2 && (name_memory | device_type_memory) & bit != 0;
                if is_memory && reg_invalid & bit != 0 {
                    return 0;
                }
                let status_ok = status_bad & bit == 0;
                if is_memory && status_ok {
                    found_memory = true;
                    if reg_dst_inside & bit != 0 {
                        destination_inside = true;
                    }
                    if reg_src_inside & bit != 0 {
                        source_inside = true;
                    }
                }

                if reserved_depth != usize::MAX {
                    if depth == reserved_depth {
                        // `size`/`alloc-ranges`/`memory-region` directly on
                        // the container describe forms this validator does not
                        // understand. Fail closed rather than guess.
                        if has_size & bit != 0
                            || has_alloc_ranges & bit != 0
                            || has_memory_region & bit != 0
                        {
                            return 0;
                        }
                    } else if depth > reserved_depth {
                        // Any descendant with a checked reg is a carve-out.
                        // A reg whose width disagrees with the container's
                        // cell layout is an unknown form: fail closed.
                        if reg_invalid & bit != 0 {
                            return 0;
                        }
                        if has_memory_region & bit != 0 {
                            return 0;
                        }
                        if (has_size | has_alloc_ranges) & bit != 0 && has_reg & bit == 0 {
                            return 0;
                        }
                        if has_reg & bit != 0 && reg_overlaps_dst & bit != 0 {
                            return 0;
                        }
                    }
                }

                if depth == reserved_depth {
                    reserved_depth = usize::MAX;
                }
                if depth == chosen_depth {
                    chosen_depth = usize::MAX;
                }
                depth -= 1;
            }
            FDT_PROP => {
                if pos
                    .checked_add(8)
                    .map_or(true, |end| end > struct_end)
                {
                    return 0;
                }
                let len = read_be32(base, pos) as usize;
                let nameoff = read_be32(base, pos + 4) as usize;
                pos += 8;

                if len > struct_end.saturating_sub(pos) {
                    return 0;
                }
                let value = pos;
                pos = align4(pos + len);
                if pos > struct_end {
                    return 0;
                }

                let name_start = match off_strings.checked_add(nameoff) {
                    Some(start) if start < strings_end => start,
                    _ => return 0,
                };
                let mut name_end = name_start;
                while name_end < strings_end && read_u8(base, name_end) != 0 {
                    name_end += 1;
                }
                if name_end >= strings_end {
                    return 0;
                }
                let name_len = name_end - name_start;
                let bit = 1_u64 << depth;

                if prop_is(base, name_start, name_len, b"device_type") {
                    if value_is_memory(base, value, len) {
                        device_type_memory |= bit;
                    }
                } else if prop_is(base, name_start, name_len, b"#address-cells") {
                    // Only the two ownership containers define a reg layout
                    // this validator consumes. Cells on /cpus or device nodes
                    // are unrelated and must not park the boot.
                    if depth == 1 {
                        match parse_cells(base, value, len) {
                            Some(cells) => root_address_cells = cells,
                            None => return 0,
                        }
                    } else if reserved_depth != usize::MAX && depth == reserved_depth {
                        match parse_cells(base, value, len) {
                            Some(cells) => reserved_address_cells = cells,
                            None => return 0,
                        }
                    }
                } else if prop_is(base, name_start, name_len, b"#size-cells") {
                    if depth == 1 {
                        match parse_cells(base, value, len) {
                            Some(cells) => root_size_cells = cells,
                            None => return 0,
                        }
                    } else if reserved_depth != usize::MAX && depth == reserved_depth {
                        match parse_cells(base, value, len) {
                            Some(cells) => reserved_size_cells = cells,
                            None => return 0,
                        }
                    }
                } else if prop_is(base, name_start, name_len, b"reg") {
                    if len != 0 {
                        // Immediate root memory children use the root
                        // layout; only descendants of /reserved-memory use
                        // the container layout. Everything else is unrelated
                        // and is not scanned, so a /cpus 1/0 reg cannot
                        // reject the whole tree.
                        let is_memory_node =
                            (name_memory | device_type_memory) & bit != 0;
                        let layout = if depth == 2 && is_memory_node {
                            Some((root_address_cells, root_size_cells))
                        } else if reserved_depth != usize::MAX && depth > reserved_depth {
                            Some((reserved_address_cells, reserved_size_cells))
                        } else {
                            None
                        };
                        if let Some((address_cells, size_cells)) = layout {
                            let (valid, inside, overlap) = scan_reg(
                                base,
                                value,
                                len,
                                address_cells,
                                size_cells,
                                dst_start,
                                dst_end,
                            );
                            if valid {
                                has_reg |= bit;
                                if inside {
                                    reg_dst_inside |= bit;
                                }
                                if overlap {
                                    reg_overlaps_dst |= bit;
                                }
                                if source_span_inside(
                                    base,
                                    value,
                                    len,
                                    address_cells,
                                    size_cells,
                                    src_start,
                                    src_end,
                                ) {
                                    reg_src_inside |= bit;
                                }
                            } else {
                                // Deferred: only memory nodes and reserved
                                // descendants treat an invalid reg as fatal.
                                reg_invalid |= bit;
                            }
                        }
                    }
                } else if prop_is(base, name_start, name_len, b"status") {
                    if status_is_okay(base, value, len) {
                        status_bad &= !bit;
                    } else {
                        status_bad |= bit;
                    }
                } else if prop_is(base, name_start, name_len, b"ranges") {
                    if reserved_depth != usize::MAX && len != 0 {
                        // Non-empty ranges need translation; this validator
                        // does not implement it. Any occurrence is fatal.
                        reserved_has_ranges = true;
                    }
                } else if prop_is(base, name_start, name_len, b"size") {
                    if reserved_depth != usize::MAX {
                        has_size |= bit;
                    }
                } else if prop_is(base, name_start, name_len, b"alloc-ranges") {
                    if reserved_depth != usize::MAX {
                        has_alloc_ranges |= bit;
                    }
                } else if prop_is(base, name_start, name_len, b"memory-region") {
                    if reserved_depth != usize::MAX {
                        // A phandle is an unknown form here: no resolution is
                        // implemented, so even a node with reg cannot be
                        // proven to describe the whole reservation.
                        has_memory_region |= bit;
                    }
                } else if chosen_depth != usize::MAX && depth == chosen_depth {
                    if prop_is(base, name_start, name_len, b"linux,initrd-start") {
                        match parse_initrd_value(base, value, len) {
                            Some(number) => initrd_start = Some(number),
                            None => return 0,
                        }
                    } else if prop_is(base, name_start, name_len, b"linux,initrd-end") {
                        match parse_initrd_value(base, value, len) {
                            Some(number) => initrd_end = Some(number),
                            None => return 0,
                        }
                    } else if name_contains(base, name_start, name_len, b"initrd") {
                        // Vendor-specific initrd properties are an unknown
                        // form. Fail closed instead of silently ignoring a
                        // region that could overlap the destination.
                        return 0;
                    }
                }
            }
            FDT_NOP => {}
            FDT_END => {
                if depth != 0 || !found_memory || !destination_inside || !source_inside {
                    return 0;
                }
                if reserved_has_ranges {
                    return 0;
                }
                if reserved_depth != usize::MAX || chosen_depth != usize::MAX {
                    return 0;
                }
                match (initrd_start, initrd_end) {
                    (None, None) => {}
                    (Some(start), Some(end)) => {
                        // A wrapped or empty range is malformed. The overlap
                        // check is against the destination only; the initrd
                        // itself is not written by this stub.
                        if start >= end || overlaps(dst_start, dst_end, start, end) {
                            return 0;
                        }
                    }
                    // One property without the other is an unknown form.
                    _ => return 0,
                }
                return 1;
            }
            _ => return 0,
        }
    }
}

unsafe fn source_span_inside(
    base: usize,
    value: usize,
    len: usize,
    address_cells: u32,
    size_cells: u32,
    src_start: u64,
    src_end: u64,
) -> bool {
    if len == 0
        || (address_cells != 1 && address_cells != 2)
        || (size_cells != 1 && size_cells != 2)
    {
        return false;
    }
    let entry = ((address_cells + size_cells) as usize) * 4;
    if len % entry != 0 {
        return false;
    }
    let mut offset = 0;
    while offset < len {
        let address = read_cells(base, value + offset, address_cells);
        let size = read_cells(base, value + offset + address_cells as usize * 4, size_cells);
        if size != 0 {
            if let Some(end) = address.checked_add(size) {
                if src_start >= address && src_end <= end {
                    return true;
                }
            }
        }
        offset += entry;
    }
    false
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn be32(value: u32) -> [u8; 4] {
        value.to_be_bytes()
    }

    fn be64(value: u64) -> [u8; 8] {
        value.to_be_bytes()
    }

    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&be32(value));
    }

    fn push_u64(out: &mut Vec<u8>, value: u64) {
        out.extend_from_slice(&be64(value));
    }

    fn push_name(out: &mut Vec<u8>, name: &[u8]) {
        out.extend_from_slice(name);
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }

    fn string_offset(strings: &mut Vec<u8>, name: &[u8]) -> u32 {
        let offset = strings.len() as u32;
        strings.extend_from_slice(name);
        strings.push(0);
        offset
    }

    fn push_prop(
        out: &mut Vec<u8>,
        strings: &mut Vec<u8>,
        name: &[u8],
        value: &[u8],
    ) {
        let nameoff = string_offset(strings, name);
        push_u32(out, FDT_PROP);
        push_u32(out, value.len() as u32);
        push_u32(out, nameoff);
        out.extend_from_slice(value);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }

    fn push_cells_prop(
        structure: &mut Vec<u8>,
        strings: &mut Vec<u8>,
        name: &[u8],
        cells: u32,
    ) {
        push_prop(structure, strings, name, &be32(cells));
    }

    /// Emits a `reg` entry using the given cell layout. Only the layouts the
    /// validator understands are used by tests; an unsupported address width
    /// still emits two address cells so the property reaches the validator.
    fn push_reg(out: &mut Vec<u8>, address: u64, size: u64, cells: (u32, u32)) {
        if cells.0 == 1 {
            push_u32(out, address as u32);
        } else {
            push_u64(out, address);
        }
        if cells.1 == 1 {
            push_u32(out, size as u32);
        } else {
            push_u64(out, size);
        }
    }

    #[derive(Clone, Copy)]
    struct MemorySpec {
        range: (u64, u64),
        status: Option<&'static str>,
    }

    impl MemorySpec {
        fn new(address: u64, size: u64) -> Self {
            Self {
                range: (address, size),
                status: None,
            }
        }

        fn with_status(mut self, status: &'static str) -> Self {
            self.status = Some(status);
            self
        }
    }

    #[derive(Clone, Copy)]
    struct ReservedSpec {
        reg: Option<(u64, u64)>,
        size: Option<u64>,
        memory_region: bool,
        nested_reg: Option<(u64, u64)>,
    }

    impl ReservedSpec {
        fn reg(address: u64, size: u64) -> Self {
            Self {
                reg: Some((address, size)),
                size: None,
                memory_region: false,
                nested_reg: None,
            }
        }

        fn size_only(size: u64) -> Self {
            Self {
                reg: None,
                size: Some(size),
                memory_region: false,
                nested_reg: None,
            }
        }

        fn memory_region() -> Self {
            Self {
                reg: None,
                size: None,
                memory_region: true,
                nested_reg: None,
            }
        }

        fn nested_reg(address: u64, size: u64) -> Self {
            Self {
                reg: None,
                size: None,
                memory_region: false,
                nested_reg: Some((address, size)),
            }
        }
    }

    #[derive(Clone, Copy)]
    struct ChosenSpec {
        start: Option<u64>,
        end: Option<u64>,
        width: u8,
        vendor_initrd: bool,
    }

    impl ChosenSpec {
        fn both(start: u64, end: u64, width: u8) -> Self {
            Self {
                start: Some(start),
                end: Some(end),
                width,
                vendor_initrd: false,
            }
        }

        fn start_only(start: u64, width: u8) -> Self {
            Self {
                start: Some(start),
                end: None,
                width,
                vendor_initrd: false,
            }
        }

        fn end_only(end: u64, width: u8) -> Self {
            Self {
                start: None,
                end: Some(end),
                width,
                vendor_initrd: false,
            }
        }
    }

    struct FdtSpec {
        memory: Vec<MemorySpec>,
        reserved: Vec<ReservedSpec>,
        memreserve: Vec<(u64, u64)>,
        chosen: Option<ChosenSpec>,
        reserved_ranges_nonempty: bool,
        root_cells: (u32, u32),
        reserved_cells: (u32, u32),
        cpus_1_0: bool,
        vendor_reg: Option<Vec<u8>>,
    }

    impl FdtSpec {
        fn base() -> Self {
            Self {
                memory: vec![MemorySpec::new(0x4000_0000, 0x0800_0000)],
                reserved: vec![],
                memreserve: vec![],
                chosen: None,
                reserved_ranges_nonempty: false,
                root_cells: (2, 2),
                reserved_cells: (2, 2),
                cpus_1_0: false,
                vendor_reg: None,
            }
        }

        fn standard() -> Self {
            let mut spec = Self::base();
            spec.reserved = vec![ReservedSpec::reg(0x47f0_0000, 0x0010_0000)];
            spec
        }
    }

    fn chosen_prop_value(number: u64, width: u8) -> Vec<u8> {
        match width {
            4 => be32(number as u32).to_vec(),
            8 => be64(number).to_vec(),
            other => vec![0u8; usize::from(other)],
        }
    }

    fn build_fdt(spec: &FdtSpec) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut structure = Vec::new();

        push_u32(&mut structure, FDT_BEGIN_NODE);
        push_name(&mut structure, b"");

        push_cells_prop(
            &mut structure,
            &mut strings,
            b"#address-cells",
            spec.root_cells.0,
        );
        push_cells_prop(
            &mut structure,
            &mut strings,
            b"#size-cells",
            spec.root_cells.1,
        );

        if spec.cpus_1_0 {
            push_u32(&mut structure, FDT_BEGIN_NODE);
            push_name(&mut structure, b"cpus");
            push_cells_prop(&mut structure, &mut strings, b"#address-cells", 1);
            push_cells_prop(&mut structure, &mut strings, b"#size-cells", 0);
            push_u32(&mut structure, FDT_BEGIN_NODE);
            push_name(&mut structure, b"cpu@0");
            push_prop(&mut structure, &mut strings, b"reg", &be32(0));
            push_u32(&mut structure, FDT_END_NODE);
            push_u32(&mut structure, FDT_END_NODE);
        }

        if let Some(value) = &spec.vendor_reg {
            push_u32(&mut structure, FDT_BEGIN_NODE);
            push_name(&mut structure, b"vendor@0");
            push_prop(&mut structure, &mut strings, b"reg", value);
            push_u32(&mut structure, FDT_END_NODE);
        }

        for (index, memory) in spec.memory.iter().enumerate() {
            let (address, size) = memory.range;
            let mut reg = Vec::new();
            push_reg(&mut reg, address, size, spec.root_cells);
            let name = std::format!("memory@{index:x}");
            push_u32(&mut structure, FDT_BEGIN_NODE);
            push_name(&mut structure, name.as_bytes());
            push_prop(&mut structure, &mut strings, b"device_type", b"memory\0");
            push_prop(&mut structure, &mut strings, b"reg", &reg);
            if let Some(status) = memory.status {
                let mut value = status.as_bytes().to_vec();
                value.push(0);
                push_prop(&mut structure, &mut strings, b"status", &value);
            }
            push_u32(&mut structure, FDT_END_NODE);
        }

        if let Some(chosen) = spec.chosen {
            push_u32(&mut structure, FDT_BEGIN_NODE);
            push_name(&mut structure, b"chosen");
            if let Some(start) = chosen.start {
                let value = chosen_prop_value(start, chosen.width);
                push_prop(&mut structure, &mut strings, b"linux,initrd-start", &value);
            }
            if let Some(end) = chosen.end {
                let value = chosen_prop_value(end, chosen.width);
                push_prop(&mut structure, &mut strings, b"linux,initrd-end", &value);
            }
            if chosen.vendor_initrd {
                let value = be32(0x4700_0000).to_vec();
                push_prop(&mut structure, &mut strings, b"qcom,initrd-start", &value);
            }
            push_u32(&mut structure, FDT_END_NODE);
        }

        push_u32(&mut structure, FDT_BEGIN_NODE);
        push_name(&mut structure, b"reserved-memory");
        push_cells_prop(
            &mut structure,
            &mut strings,
            b"#address-cells",
            spec.reserved_cells.0,
        );
        push_cells_prop(
            &mut structure,
            &mut strings,
            b"#size-cells",
            spec.reserved_cells.1,
        );
        if spec.reserved_ranges_nonempty {
            let mut ranges = Vec::new();
            push_u64(&mut ranges, 0x4000_0000);
            push_u64(&mut ranges, 0x1000);
            push_prop(&mut structure, &mut strings, b"ranges", &ranges);
        } else {
            push_prop(&mut structure, &mut strings, b"ranges", &[]);
        }
        for (index, reserved) in spec.reserved.iter().enumerate() {
            let name = std::format!("reserved@{index}");
            push_u32(&mut structure, FDT_BEGIN_NODE);
            push_name(&mut structure, name.as_bytes());
            if let Some((address, size)) = reserved.reg {
                let mut reg = Vec::new();
                push_reg(&mut reg, address, size, spec.reserved_cells);
                push_prop(&mut structure, &mut strings, b"reg", &reg);
            }
            if let Some(size) = reserved.size {
                let value = be64(size).to_vec();
                push_prop(&mut structure, &mut strings, b"size", &value);
            }
            if reserved.memory_region {
                let value = be32(1).to_vec();
                push_prop(&mut structure, &mut strings, b"memory-region", &value);
            }
            if let Some((address, size)) = reserved.nested_reg {
                push_u32(&mut structure, FDT_BEGIN_NODE);
                push_name(&mut structure, b"nested@0");
                let mut reg = Vec::new();
                push_reg(&mut reg, address, size, spec.reserved_cells);
                push_prop(&mut structure, &mut strings, b"reg", &reg);
                push_u32(&mut structure, FDT_END_NODE);
            }
            push_u32(&mut structure, FDT_END_NODE);
        }
        push_u32(&mut structure, FDT_END_NODE);
        push_u32(&mut structure, FDT_END_NODE);
        push_u32(&mut structure, FDT_END);

        let mut memreserve = Vec::new();
        for &(address, size) in &spec.memreserve {
            push_u64(&mut memreserve, address);
            push_u64(&mut memreserve, size);
        }
        push_u64(&mut memreserve, 0);
        push_u64(&mut memreserve, 0);

        let off_rsv = FDT_HEADER_SIZE;
        let off_strings = off_rsv + memreserve.len();
        let off_struct = align4(off_strings + strings.len());
        let total = off_struct + structure.len();

        let mut out = Vec::with_capacity(total);
        push_u32(&mut out, FDT_MAGIC);
        push_u32(&mut out, total as u32);
        push_u32(&mut out, off_struct as u32);
        push_u32(&mut out, off_strings as u32);
        push_u32(&mut out, off_rsv as u32);
        push_u32(&mut out, 17);
        push_u32(&mut out, 16);
        push_u32(&mut out, 0);
        push_u32(&mut out, strings.len() as u32);
        push_u32(&mut out, structure.len() as u32);
        assert_eq!(out.len(), FDT_HEADER_SIZE);
        out.extend_from_slice(&memreserve);
        out.extend_from_slice(&strings);
        while out.len() < off_struct {
            out.push(0);
        }
        out.extend_from_slice(&structure);
        assert_eq!(out.len(), total);
        out
    }

    fn validate(
        fdt: &[u8],
        dst_start: u64,
        dst_end: u64,
        src_start: u64,
        src_end: u64,
    ) -> u64 {
        unsafe {
            validate_relocation_inner(
                fdt.as_ptr() as usize,
                dst_start,
                dst_end,
                src_start,
                src_end,
                u64::MAX,
            )
        }
    }

    fn validate_legacy(
        fdt: &[u8],
        dst_start: u64,
        dst_end: u64,
        src_start: u64,
        src_end: u64,
    ) -> u64 {
        unsafe {
            legacy::validate_relocation_inner_old(
                fdt.as_ptr() as usize,
                dst_start,
                dst_end,
                src_start,
                src_end,
                u64::MAX,
            )
        }
    }

    fn standard() -> Vec<u8> {
        build_fdt(&FdtSpec::standard())
    }

    #[test]
    fn dram_window_policy_is_fail_closed() {
        assert!(in_dram_span(0x4000_0000, 0x4000_1000));
        assert!(in_dram_span(0x7fff_f000, 0x8000_0000));
        assert!(!in_dram_span(0x3fff_f000, 0x4000_0000));
        assert!(!in_dram_span(0x7fff_f000, 0x8000_1000));
        assert!(!in_dram_span(0x4000_0000, 0x4000_0000));
    }

    #[test]
    fn accepts_destination_inside_memory() {
        let fdt = standard();
        assert_eq!(
            validate(&fdt, 0x4008_0000, 0x401a_e000, 0x4040_0000, 0x405a_e000),
            1
        );
    }

    #[test]
    fn rejects_destination_outside_memory() {
        let fdt = standard();
        assert_eq!(
            validate(&fdt, 0x3000_0000, 0x3000_1000, 0x4040_0000, 0x4040_1000),
            0
        );
    }

    #[test]
    fn rejects_destination_span_crossing_memory_end() {
        let fdt = standard();
        assert_eq!(
            validate(&fdt, 0x47ff_f000, 0x4800_1000, 0x4040_0000, 0x4040_1000),
            0
        );
    }

    #[test]
    fn rejects_reserved_memory_overlap() {
        let fdt = standard();
        assert_eq!(
            validate(&fdt, 0x47f0_0000, 0x47f0_1000, 0x4040_0000, 0x4040_1000),
            0
        );
    }

    #[test]
    fn rejects_memreserve_overlap() {
        let mut spec = FdtSpec::base();
        spec.memreserve = vec![(0x4008_0000, 0x0020_0000)];
        let fdt = build_fdt(&spec);
        assert_eq!(
            validate(&fdt, 0x4008_0000, 0x400a_0000, 0x4040_0000, 0x4042_0000),
            0
        );
    }

    #[test]
    fn rejects_source_overlap_and_source_outside_memory() {
        let fdt = standard();
        assert_eq!(
            validate(&fdt, 0x4008_0000, 0x4010_0000, 0x400f_0000, 0x4010_0000),
            0
        );
        assert_eq!(
            validate(&fdt, 0x4008_0000, 0x4010_0000, 0x3000_0000, 0x3000_1000),
            0
        );
    }

    #[test]
    fn rejects_bad_magic_and_short_tree() {
        let mut fdt = standard();
        fdt[0] = 0;
        assert_eq!(
            validate(&fdt, 0x4008_0000, 0x4010_0000, 0x4040_0000, 0x4050_0000),
            0
        );
    }

    #[test]
    fn rejects_missing_memory_node() {
        let mut spec = FdtSpec::base();
        spec.memory.clear();
        let fdt = build_fdt(&spec);
        assert_eq!(
            validate(&fdt, 0x4008_0000, 0x4010_0000, 0x4040_0000, 0x4050_0000),
            0
        );
    }

    #[test]
    fn root_cells_value_three_is_rejected() {
        // The root defines its children's reg layout. A value the validator
        // does not understand must fail closed, not fall back to 2/2.
        let mut spec = FdtSpec::base();
        spec.root_cells = (3, 2);
        assert_eq!(run_both(&spec), (0, 0));
    }

    const DST_START: u64 = 0x4008_0000;
    const DST_END: u64 = 0x4010_0000;
    const SRC_START: u64 = 0x4040_0000;
    const SRC_END: u64 = 0x4050_0000;

    /// Runs the new validator and the pre-A2 oracle. `(new, legacy)`.
    fn run_both(spec: &FdtSpec) -> (u64, u64) {
        let fdt = build_fdt(spec);
        (
            validate(&fdt, DST_START, DST_END, SRC_START, SRC_END),
            validate_legacy(&fdt, DST_START, DST_END, SRC_START, SRC_END),
        )
    }

    #[test]
    fn cpus_1_0_does_not_block_relocation() {
        // A normal /cpus node uses 1 address cell and 0 size cells. The old
        // global 2/2 rule rejected the tree; only ownership nodes may decide.
        let mut spec = FdtSpec::base();
        spec.cpus_1_0 = true;
        assert_eq!(run_both(&spec), (1, 0));
    }

    #[test]
    fn unrelated_reg_is_not_interpreted() {
        // A malformed reg on an unrelated root child must not be treated as
        // ownership data. The legacy oracle also scoped its scan to memory and
        // reserved children, so both accept; the regression is that the new
        // cell handling must not widen that scan to unrelated nodes.
        let mut spec = FdtSpec::base();
        spec.vendor_reg = Some(vec![0xDE, 0xAD, 0xBE]);
        assert_eq!(run_both(&spec), (1, 1));
    }

    #[test]
    fn memory_1_1_uses_root_cells() {
        let mut spec = FdtSpec::base();
        spec.root_cells = (1, 1);
        assert_eq!(run_both(&spec), (1, 0));
    }

    #[test]
    fn memory_1_1_outside_destination_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.root_cells = (1, 1);
        let fdt = build_fdt(&spec);
        assert_eq!(
            validate(&fdt, 0x3000_0000, 0x3000_1000, SRC_START, SRC_END),
            0
        );
    }

    #[test]
    fn memory_1_1_reserved_overlap_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.root_cells = (1, 1);
        spec.reserved_cells = (1, 1);
        spec.reserved = vec![ReservedSpec::reg(0x4008_1000, 0x1000)];
        assert_eq!(run_both(&spec), (0, 0));
    }

    #[test]
    fn reserved_memory_1_1_is_parsed() {
        // The reserved-memory container, not the root, defines its children's
        // reg layout. A 1/1 reservation outside the destination is safe.
        let mut spec = FdtSpec::base();
        spec.reserved_cells = (1, 1);
        spec.reserved = vec![ReservedSpec::reg(0x4700_0000, 0x1000)];
        assert_eq!(run_both(&spec), (1, 0));
    }

    #[test]
    fn reserved_memory_1_1_overlap_is_rejected() {
        // Proves the 1/1 size is actually decoded, not treated as 2/2.
        let mut spec = FdtSpec::base();
        spec.reserved_cells = (1, 1);
        spec.reserved = vec![ReservedSpec::reg(0x4008_1000, 0x1000)];
        assert_eq!(run_both(&spec), (0, 0));
    }

    #[test]
    fn memory_reg_width_disagreeing_with_root_is_rejected() {
        // Root says 2/2, so a 1/1 memory reg (8 bytes per entry) is malformed
        // ownership data and must fail closed.
        let fdt = {
            // A minimal tree whose only memory reg uses the wrong layout.
            let mut strings = Vec::new();
            let mut structure = Vec::new();
            push_u32(&mut structure, FDT_BEGIN_NODE);
            push_name(&mut structure, b"");
            push_cells_prop(&mut structure, &mut strings, b"#address-cells", 2);
            push_cells_prop(&mut structure, &mut strings, b"#size-cells", 2);
            push_u32(&mut structure, FDT_BEGIN_NODE);
            push_name(&mut structure, b"memory@40080000");
            push_prop(&mut structure, &mut strings, b"device_type", b"memory\0");
            let mut reg = Vec::new();
            push_reg(&mut reg, 0x4008_0000, 0x1000, (1, 1));
            push_prop(&mut structure, &mut strings, b"reg", &reg);
            push_u32(&mut structure, FDT_END_NODE);
            push_u32(&mut structure, FDT_END_NODE);
            push_u32(&mut structure, FDT_END);
            let memreserve = [0u8; 16];
            let off_rsv = FDT_HEADER_SIZE;
            let off_strings = off_rsv + memreserve.len();
            let off_struct = align4(off_strings + strings.len());
            let total = off_struct + structure.len();
            let mut out = Vec::new();
            push_u32(&mut out, FDT_MAGIC);
            push_u32(&mut out, total as u32);
            push_u32(&mut out, off_struct as u32);
            push_u32(&mut out, off_strings as u32);
            push_u32(&mut out, off_rsv as u32);
            push_u32(&mut out, 17);
            push_u32(&mut out, 16);
            push_u32(&mut out, 0);
            push_u32(&mut out, strings.len() as u32);
            push_u32(&mut out, structure.len() as u32);
            out.extend_from_slice(&memreserve);
            out.extend_from_slice(&strings);
            while out.len() < off_struct {
                out.push(0);
            }
            out.extend_from_slice(&structure);
            out
        };
        assert_eq!(validate(&fdt, DST_START, DST_END, SRC_START, SRC_END), 0);
    }

    #[test]
    fn initrd_overlapping_destination_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.chosen = Some(ChosenSpec::both(0x4008_1000, 0x4008_3000, 8));
        // The old validator ignored /chosen entirely and accepted this tree.
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn initrd_non_overlapping_destination_is_accepted() {
        let mut spec = FdtSpec::base();
        spec.chosen = Some(ChosenSpec::both(0x4700_0000, 0x4710_0000, 8));
        assert_eq!(run_both(&spec), (1, 1));
    }

    #[test]
    fn initrd_with_only_start_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.chosen = Some(ChosenSpec::start_only(0x4700_0000, 8));
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn initrd_with_only_end_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.chosen = Some(ChosenSpec::end_only(0x4710_0000, 8));
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn initrd_wrapped_range_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.chosen = Some(ChosenSpec::both(0xFFFF_FFFF_FFFF_F000, 0x1000, 8));
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn initrd_unparseable_width_is_rejected() {
        let mut spec = FdtSpec::base();
        // Two bytes is neither one 32-bit nor one 64-bit cell.
        spec.chosen = Some(ChosenSpec::both(0x4000_0000, 0x4000_1000, 2));
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn vendor_initrd_property_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.chosen = Some(ChosenSpec {
            start: None,
            end: None,
            width: 8,
            vendor_initrd: true,
        });
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn initrd_32bit_non_overlapping_range_is_accepted() {
        let mut spec = FdtSpec::base();
        spec.chosen = Some(ChosenSpec::both(0x4700_0000, 0x4710_0000, 4));
        assert_eq!(run_both(&spec), (1, 1));
    }

    #[test]
    fn initrd_empty_range_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.chosen = Some(ChosenSpec::both(0x4700_0000, 0x4700_0000, 8));
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn nested_reserved_memory_reg_outside_destination_is_accepted() {
        let mut spec = FdtSpec::base();
        spec.reserved = vec![ReservedSpec::nested_reg(0x4700_0000, 0x1000)];
        assert_eq!(run_both(&spec), (1, 1));
    }

    #[test]
    fn reserved_memory_size_with_checked_reg_is_accepted() {
        let mut spec = FdtSpec::base();
        spec.reserved = vec![ReservedSpec {
            reg: Some((0x4700_0000, 0x1000)),
            size: Some(0x1000),
            memory_region: false,
            nested_reg: None,
        }];
        assert_eq!(run_both(&spec), (1, 1));
    }

    #[test]
    fn nested_reserved_memory_reg_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.reserved = vec![ReservedSpec::nested_reg(0x4008_1000, 0x1000)];
        // The old validator only looked at direct children of reserved-memory.
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn reserved_memory_size_without_reg_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.reserved = vec![ReservedSpec::size_only(0x1000)];
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn reserved_memory_memory_region_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.reserved = vec![ReservedSpec::memory_region()];
        // Phandle resolution is not implemented; unknown form must fail.
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn reserved_memory_nonempty_ranges_is_rejected() {
        let mut spec = FdtSpec::base();
        spec.reserved_ranges_nonempty = true;
        // No child has reg, so the old conditional check accepted the tree.
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn disabled_memory_is_not_usable() {
        let mut spec = FdtSpec::base();
        spec.memory = vec![MemorySpec::new(0x4000_0000, 0x0800_0000).with_status("disabled")];
        // The old validator ignored status and accepted the reg as RAM.
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn unknown_memory_status_is_not_usable() {
        let mut spec = FdtSpec::base();
        spec.memory = vec![MemorySpec::new(0x4000_0000, 0x0800_0000).with_status("reserved")];
        assert_eq!(run_both(&spec), (0, 1));
    }

    #[test]
    fn okay_memory_is_accepted() {
        let mut spec = FdtSpec::base();
        spec.memory = vec![MemorySpec::new(0x4000_0000, 0x0800_0000).with_status("okay")];
        assert_eq!(run_both(&spec), (1, 1));
    }

    #[test]
    fn normal_dtb_is_accepted() {
        assert_eq!(run_both(&FdtSpec::standard()), (1, 1));
    }

    #[test]
    fn legacy_still_rejects_memreserve_and_reserved_overlap() {
        let fdt = standard();
        assert_eq!(
            validate_legacy(&fdt, 0x47f0_0000, 0x47f0_1000, SRC_START, 0x4040_1000),
            0
        );
        let mut spec = FdtSpec::base();
        spec.memreserve = vec![(DST_START, 0x0020_0000)];
        let fdt = build_fdt(&spec);
        assert_eq!(
            validate_legacy(&fdt, DST_START, 0x400a_0000, SRC_START, 0x4042_0000),
            0
        );
    }

    /// Test-only copy of the pre-A2 validator. The negative tests above assert
    /// that this oracle accepts the tree while the new validator rejects it;
    /// that is the regression proof, not a tautology.
    #[allow(dead_code)]
    mod legacy {
        use super::super::*;

        pub unsafe fn validate_relocation_inner_old(
            base: usize,
            dst_start: u64,
            dst_end: u64,
            src_start: u64,
            src_end: u64,
            limit: u64,
        ) -> u64 {
            if dst_start >= dst_end || src_start >= src_end {
                return 0;
            }

            let totalsize = read_be32(base, 4) as usize;
            if read_be32(base, 0) != FDT_MAGIC || totalsize < FDT_HEADER_SIZE {
                return 0;
            }
            let base_end = match base.checked_add(totalsize) {
                Some(end) if end as u64 <= limit => end,
                _ => return 0,
            };
            let dtb_start = base as u64;
            let dtb_end = base_end as u64;

            if overlaps(dst_start, dst_end, src_start, src_end)
                || overlaps(dst_start, dst_end, dtb_start, dtb_end)
            {
                return 0;
            }

            let off_struct = read_be32(base, 8) as usize;
            let off_strings = read_be32(base, 12) as usize;
            let off_rsv = read_be32(base, 16) as usize;
            let size_strings = read_be32(base, 32) as usize;
            let size_struct = read_be32(base, 36) as usize;

            let struct_end = match off_struct.checked_add(size_struct) {
                Some(end) if end <= totalsize => end,
                _ => return 0,
            };
            let strings_end = match off_strings.checked_add(size_strings) {
                Some(end) if end <= totalsize => end,
                _ => return 0,
            };
            if off_rsv >= totalsize {
                return 0;
            }

            let mut rsv = off_rsv;
            let mut rsv_count = 0;
            loop {
                if rsv_count >= MAX_MEMRESERVE
                    || rsv.checked_add(16).map_or(true, |end| end > totalsize)
                {
                    return 0;
                }
                let address = read_be64(base, rsv);
                let size = read_be64(base, rsv + 8);
                if address == 0 && size == 0 {
                    break;
                }
                if size != 0 {
                    let end = match address.checked_add(size) {
                        Some(end) => end,
                        None => return 0,
                    };
                    if overlaps(dst_start, dst_end, address, end) {
                        return 0;
                    }
                }
                rsv += 16;
                rsv_count += 1;
            }

            let mut pos = off_struct;
            let mut depth: usize = 0;
            let mut memory_mask: u64 = 0;
            let mut reserved_depth: usize = usize::MAX;
            let mut found_memory = false;
            let mut destination_inside = false;
            let mut source_inside = false;
            let mut reserved_has_ranges = false;
            let mut reserved_child_seen = false;

            loop {
                if pos.checked_add(4).map_or(true, |end| end > struct_end) {
                    return 0;
                }
                let token = read_be32(base, pos);
                pos += 4;

                match token {
                    FDT_BEGIN_NODE => {
                        if depth >= MAX_DEPTH {
                            return 0;
                        }
                        let name_start = pos;
                        while pos < struct_end && read_u8(base, pos) != 0 {
                            pos += 1;
                        }
                        if pos >= struct_end {
                            return 0;
                        }
                        let name_len = pos - name_start;
                        pos = align4(pos + 1);
                        if pos > struct_end {
                            return 0;
                        }
                        depth += 1;
                        if node_is_memory(base, name_start, name_len) {
                            memory_mask |= 1_u64 << depth;
                        }
                        if node_is_reserved_memory(base, name_start, name_len) {
                            reserved_depth = depth;
                        }
                    }
                    FDT_END_NODE => {
                        if depth == 0 {
                            return 0;
                        }
                        if depth == reserved_depth {
                            reserved_depth = usize::MAX;
                        }
                        memory_mask &= !(1_u64 << depth);
                        depth -= 1;
                    }
                    FDT_PROP => {
                        if pos.checked_add(8).map_or(true, |end| end > struct_end) {
                            return 0;
                        }
                        let len = read_be32(base, pos) as usize;
                        let nameoff = read_be32(base, pos + 4) as usize;
                        pos += 8;

                        if len > struct_end.saturating_sub(pos) {
                            return 0;
                        }
                        let value = pos;
                        pos = align4(pos + len);
                        if pos > struct_end {
                            return 0;
                        }

                        let name_start = match off_strings.checked_add(nameoff) {
                            Some(start) if start < strings_end => start,
                            _ => return 0,
                        };
                        let mut name_end = name_start;
                        while name_end < strings_end && read_u8(base, name_end) != 0 {
                            name_end += 1;
                        }
                        if name_end >= strings_end {
                            return 0;
                        }
                        let name_len = name_end - name_start;

                        if prop_is(base, name_start, name_len, b"device_type") {
                            if value_is_memory(base, value, len) {
                                memory_mask |= 1_u64 << depth;
                            }
                        } else if prop_is(base, name_start, name_len, b"#address-cells")
                            || prop_is(base, name_start, name_len, b"#size-cells")
                        {
                            if len != 4 || read_be32(base, value) != 2 {
                                return 0;
                            }
                        } else if prop_is(base, name_start, name_len, b"reg") {
                            let is_memory = (memory_mask & (1_u64 << depth)) != 0;
                            let is_reserved_child =
                                reserved_depth != usize::MAX && depth == reserved_depth + 1;
                            if is_memory || is_reserved_child {
                                let (valid, inside, overlap) =
                                    scan_reg(base, value, len, 2, 2, dst_start, dst_end);
                                if !valid {
                                    return 0;
                                }
                                if is_memory {
                                    found_memory = true;
                                    if inside {
                                        destination_inside = true;
                                    }
                                    if source_span_inside(base, value, len, 2, 2, src_start, src_end)
                                    {
                                        source_inside = true;
                                    }
                                }
                                if is_reserved_child {
                                    reserved_child_seen = true;
                                    if overlap {
                                        return 0;
                                    }
                                }
                            }
                        } else if prop_is(base, name_start, name_len, b"ranges")
                            && reserved_depth != usize::MAX
                            && depth == reserved_depth
                            && len != 0
                        {
                            reserved_has_ranges = true;
                        }
                    }
                    FDT_NOP => {}
                    FDT_END => {
                        if depth != 0 || !found_memory || !destination_inside || !source_inside {
                            return 0;
                        }
                        if reserved_has_ranges && reserved_child_seen {
                            return 0;
                        }
                        return 1;
                    }
                    _ => return 0,
                }
            }
        }
    }
}
