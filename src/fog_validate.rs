//! Pre-relocation FDT validation for the fog self-relocation.
//!
//! This function runs from the boot stub **before** the image is copied: the
//! MMU is off, the image may still be at the address ABL chose, and there is
//! no allocator or Rust runtime. It therefore:
//!
//! * takes only the DTB pointer and the source/destination spans,
//! * reads the flattened device tree with raw big-endian loads,
//! * uses no statics, no allocation, no formatting and no panics,
//! * returns `1` only when the destination span is provably inside a
//!   `/memory` region and outside every memreserve, `/reserved-memory` child,
//!   the DTB itself and the running source image.
//!
//! Any malformed tree, unsupported cell layout, non-empty `ranges` in
//! `/reserved-memory`, or missing memory node returns `0` (fail closed).
//! The caller parks instead of copying.
//!
//! The policy is intentionally conservative for the first Redmi 10C PoC:
//! `#address-cells` and `#size-cells` must both be 2. The khaje tree and the
//! synthetic QEMU tree use 2/2. If a real unit uses another layout, the boot
//! parks and the tree can be inspected; it never guesses.

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

/// Scans one `reg` value (2 address cells + 2 size cells per entry).
///
/// Returns `(valid, destination_inside, destination_overlaps)`.
unsafe fn scan_reg(
    base: usize,
    value: usize,
    len: usize,
    dst_start: u64,
    dst_end: u64,
) -> (bool, bool, bool) {
    if len == 0 {
        return (true, false, false);
    }
    if len % 16 != 0 {
        return (false, false, false);
    }
    let mut inside = false;
    let mut overlap = false;
    let mut offset = 0;
    while offset < len {
        let address = read_be64(base, value + offset);
        let size = read_be64(base, value + offset + 8);
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
        offset += 16;
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

                if prop_is(base, name_start, name_len, b"device_type") {
                    if value_is_memory(base, value, len) {
                        memory_mask |= 1_u64 << depth;
                    }
                } else if prop_is(base, name_start, name_len, b"#address-cells")
                    || prop_is(base, name_start, name_len, b"#size-cells")
                {
                    // The first PoC only supports the standard 2/2 layout.
                    if len != 4 || read_be32(base, value) != 2 {
                        return 0;
                    }
                } else if prop_is(base, name_start, name_len, b"reg") {
                    let is_memory = (memory_mask & (1_u64 << depth)) != 0;
                    let is_reserved_child =
                        reserved_depth != usize::MAX && depth == reserved_depth + 1;
                    if is_memory || is_reserved_child {
                        let (valid, inside, overlap) =
                            scan_reg(base, value, len, dst_start, dst_end);
                        if !valid {
                            return 0;
                        }
                        if is_memory {
                            found_memory = true;
                            if inside {
                                destination_inside = true;
                            }
                            // The source span must also be inside a `/memory`
                            // region; `scan_reg` only reports the destination.
                            if source_span_inside(base, value, len, src_start, src_end) {
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
                    // Non-empty ranges would require translation of the child
                    // reg values; this PoC does not implement it.
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

unsafe fn source_span_inside(
    base: usize,
    value: usize,
    len: usize,
    src_start: u64,
    src_end: u64,
) -> bool {
    if len == 0 || len % 16 != 0 {
        return false;
    }
    let mut offset = 0;
    while offset < len {
        let address = read_be64(base, value + offset);
        let size = read_be64(base, value + offset + 8);
        if size != 0 {
            if let Some(end) = address.checked_add(size) {
                if src_start >= address && src_end <= end {
                    return true;
                }
            }
        }
        offset += 16;
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

    struct FdtSpec {
        memory: Vec<(u64, u64)>,
        reserved: Vec<(u64, u64)>,
        memreserve: Vec<(u64, u64)>,
    }

    fn build_fdt(spec: &FdtSpec) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut structure = Vec::new();

        push_u32(&mut structure, FDT_BEGIN_NODE);
        push_name(&mut structure, b"");

        let cells = be32(2);
        push_prop(&mut structure, &mut strings, b"#address-cells", &cells);
        push_prop(&mut structure, &mut strings, b"#size-cells", &cells);

        for &(address, size) in &spec.memory {
            let mut reg = Vec::new();
            push_u64(&mut reg, address);
            push_u64(&mut reg, size);
            push_u32(&mut structure, FDT_BEGIN_NODE);
            push_name(&mut structure, b"memory");
            push_prop(&mut structure, &mut strings, b"device_type", b"memory\0");
            push_prop(&mut structure, &mut strings, b"reg", &reg);
            push_u32(&mut structure, FDT_END_NODE);
        }

        push_u32(&mut structure, FDT_BEGIN_NODE);
        push_name(&mut structure, b"reserved-memory");
        push_prop(&mut structure, &mut strings, b"#address-cells", &cells);
        push_prop(&mut structure, &mut strings, b"#size-cells", &cells);
        push_prop(&mut structure, &mut strings, b"ranges", &[]);
        for (index, &(address, size)) in spec.reserved.iter().enumerate() {
            let mut reg = Vec::new();
            push_u64(&mut reg, address);
            push_u64(&mut reg, size);
            let name = std::format!("reserved@{index}");
            push_u32(&mut structure, FDT_BEGIN_NODE);
            push_name(&mut structure, name.as_bytes());
            push_prop(&mut structure, &mut strings, b"reg", &reg);
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

    fn standard() -> Vec<u8> {
        build_fdt(&FdtSpec {
            memory: vec![(0x4000_0000, 0x0800_0000)],
            reserved: vec![(0x47f0_0000, 0x0010_0000)],
            memreserve: vec![],
        })
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
        let fdt = build_fdt(&FdtSpec {
            memory: vec![(0x4000_0000, 0x0800_0000)],
            reserved: vec![],
            memreserve: vec![(0x4008_0000, 0x0020_0000)],
        });
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
        let fdt = build_fdt(&FdtSpec {
            memory: vec![],
            reserved: vec![],
            memreserve: vec![],
        });
        assert_eq!(
            validate(&fdt, 0x4008_0000, 0x4010_0000, 0x4040_0000, 0x4050_0000),
            0
        );
    }

    #[test]
    fn rejects_unsupported_cell_layout() {
        // A 1/1 root is valid FDT but outside this first-PoC validator's
        // supported layout; it must fail closed, not guess.
        let mut strings = Vec::new();
        let mut structure = Vec::new();
        push_u32(&mut structure, FDT_BEGIN_NODE);
        push_name(&mut structure, b"");
        push_prop(&mut structure, &mut strings, b"#address-cells", &be32(1));
        push_prop(&mut structure, &mut strings, b"#size-cells", &be32(1));
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
        assert_eq!(
            validate(&out, 0x4008_0000, 0x4010_0000, 0x4040_0000, 0x4050_0000),
            0
        );
    }
}
