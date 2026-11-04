#![cfg_attr(not(test), no_std)]

//! Shared pure logic of the Silver microkernel.
//!
//! This module is deliberately free of kernel state so the placement
//! arithmetic can be regression-tested on the host (`cargo test --lib`)
//! while the no_std kernel links the very same code.

extern crate alloc;

pub mod fdt;
pub mod fog_validate;
pub mod gicv3_policy;
pub mod irq_policy;
pub mod phys_range;
pub mod reloc;
pub mod table_registry;
pub mod wait_queue;
pub mod watchdog_policy;

use alloc::vec::Vec;

/// Version tag of the `SYS_SPAWN_ARGSV` argument block.
pub const SPAWN_ARGS_VERSION: u32 = 1;
/// Hard cap on `argc` and `envc`, independent of the byte cap.
pub const MAX_SPAWN_ARGS: usize = 32;

/// Parsed `SYS_SPAWN_ARGSV` block: the strings the new process will receive
/// as `argv` and `envp`.
pub struct SpawnArgs {
    pub argv: Vec<Vec<u8>>,
    pub envp: Vec<Vec<u8>>,
}

/// Parses the versioned spawn-arguments block:
///
/// ```text
/// u32 version (= 1) | u32 argc | u32 envc | u32 argv_off | u32 envp_off | u32 total_len
/// then NUL-terminated strings: argv strings, then envp strings
/// ```
///
/// Everything is bounded and every offset is validated before slicing, so a
/// corrupted userspace block can only fail closed. `total_len` must equal the
/// copied block length and `argv_off`/`envp_off` must be ordered, which rules
/// out overlapping tables.
pub fn parse_spawn_args(block: &[u8]) -> Option<SpawnArgs> {
    if block.len() < 24 {
        return None;
    }
    let version = read_u32(block, 0)?;
    if version != SPAWN_ARGS_VERSION {
        return None;
    }
    let argc = usize::try_from(read_u32(block, 4)?).ok()?;
    let envc = usize::try_from(read_u32(block, 8)?).ok()?;
    if argc > MAX_SPAWN_ARGS || envc > MAX_SPAWN_ARGS {
        return None;
    }
    let argv_off = usize::try_from(read_u32(block, 12)?).ok()?;
    let envp_off = usize::try_from(read_u32(block, 16)?).ok()?;
    let total = usize::try_from(read_u32(block, 20)?).ok()?;
    if total != block.len() || argv_off < 24 || envp_off < argv_off || envp_off > total {
        return None;
    }
    let argv = parse_c_strings(&block[argv_off..envp_off], argc)?;
    let envp = parse_c_strings(&block[envp_off..total], envc)?;
    Some(SpawnArgs { argv, envp })
}

fn read_u32(block: &[u8], offset: usize) -> Option<u32> {
    let bytes = block.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn parse_c_strings(mut data: &[u8], count: usize) -> Option<Vec<Vec<u8>>> {
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        let end = data.iter().position(|&byte| byte == 0)?;
        result.push(data[..end].to_vec());
        data = &data[end + 1..];
    }
    Some(result)
}

/// A ready-to-copy initial user stack block plus the new user SP.
pub struct ArgvEnvBlock {
    pub block: Vec<u8>,
    pub sp: usize,
}

/// Builds the classic `argc/argv/envp` stack image at the top of the user
/// stack. The block is laid out low-to-high as:
///
/// ```text
/// [argc][argv[0]..argv[n-1]][NULL][envp[0]..envp[m-1]][NULL][strings...]
/// ```
///
/// `sp` is 16-byte aligned (the AArch64 procedure-call boundary) and the block
/// ends exactly at `stack_top`. Returns `None` for embedded NULs, more than
/// [`MAX_SPAWN_ARGS`] entries, arithmetic overflow, or when the block does not
/// fit `max_bytes`.
pub fn build_argv_envp(
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    stack_top: usize,
    max_bytes: usize,
) -> Option<ArgvEnvBlock> {
    if argv.len() > MAX_SPAWN_ARGS || envp.len() > MAX_SPAWN_ARGS {
        return None;
    }
    if argv.iter().chain(envp.iter()).any(|text| text.contains(&0)) {
        return None;
    }
    let mut strings_len = 0usize;
    for text in argv.iter().chain(envp.iter()) {
        strings_len = strings_len.checked_add(text.len().checked_add(1)?)?;
    }
    let pointers = (argv.len() + 1 + envp.len() + 1).checked_mul(8)?;
    let raw = 8usize.checked_add(pointers)?.checked_add(strings_len)?;
    let total = raw.checked_add(15)? & !15;
    if total == 0 || total > max_bytes {
        return None;
    }
    let sp = stack_top.checked_sub(total)?;
    let mut block = alloc::vec![0u8; total];
    block[0..8].copy_from_slice(&(argv.len() as u64).to_le_bytes());

    let argv_ptrs = 8usize;
    let envp_ptrs = argv_ptrs.checked_add((argv.len() + 1).checked_mul(8)?)?;
    let strings = envp_ptrs.checked_add((envp.len() + 1).checked_mul(8)?)?;
    let mut cursor = strings;
    for (index, text) in argv.iter().enumerate() {
        let virtual_address = sp.checked_add(cursor)?;
        let slot = argv_ptrs.checked_add(index.checked_mul(8)?)?;
        block[slot..slot + 8].copy_from_slice(&(virtual_address as u64).to_le_bytes());
        block[cursor..cursor + text.len()].copy_from_slice(text);
        cursor = cursor.checked_add(text.len().checked_add(1)?)?;
    }
    for (index, text) in envp.iter().enumerate() {
        let virtual_address = sp.checked_add(cursor)?;
        let slot = envp_ptrs.checked_add(index.checked_mul(8)?)?;
        block[slot..slot + 8].copy_from_slice(&(virtual_address as u64).to_le_bytes());
        block[cursor..cursor + text.len()].copy_from_slice(text);
        cursor = cursor.checked_add(text.len().checked_add(1)?)?;
    }
    Some(ArgvEnvBlock { block, sp })
}

/// Pure first-fit placement for anonymous `SYS_MMAP` requests.
///
/// Everything attacker-controlled (`length`) is bounded or checked before any
/// arithmetic that could overflow `usize`:
///
/// 1. the requested page count is rejected up front when it cannot fit the
///    `[min_va, max_va)` window at all, so the span multiplication below is
///    guaranteed small (checked anyway as a second line of defence);
/// 2. every span is computed with `checked_mul` / `checked_add`.
///
/// Returns the placed virtual address and the page count derived from
/// `length`, or `None` when the request is impossible: zero length, a span
/// that does not fit the window, or no free gap. Never panics.
pub fn mmap_place<I>(
    length: usize,
    min_va: usize,
    max_va: usize,
    page_size: usize,
    regions: I,
) -> Option<(usize, usize)>
where
    I: Iterator<Item = (usize, usize)>,
{
    if page_size == 0 || min_va >= max_va || max_va % page_size != 0 || min_va % page_size != 0 {
        return None;
    }
    // Explicit upper bound before any span arithmetic: a mapping must fit the
    // window, so anything larger is rejected without computing a span at all.
    // For `length = usize::MAX` this is what prevents the historical
    // `page_count * page_size` overflow.
    let max_pages = (max_va - min_va) / page_size;
    let page_count = length.div_ceil(page_size);
    if page_count == 0 || page_count > max_pages {
        return None;
    }
    let span = page_count.checked_mul(page_size)?;

    // First-fit over the sorted, non-overlapping region list.
    let mut cursor = min_va;
    for (virtual_address, region_pages) in regions {
        let region_end = virtual_address.checked_add(region_pages.checked_mul(page_size)?)?;
        if cursor.checked_add(span)? <= virtual_address {
            break;
        }
        cursor = region_end;
    }
    let virtual_address = cursor;
    if virtual_address.checked_add(span)? > max_va {
        return None;
    }
    Some((virtual_address, page_count))
}

/// Per-task anonymous-mapping quota check: a request may grow the task's
/// `current` total by `request` bytes only while the result stays within
/// `limit`. Overflow and a zero-length request are refused.
///
/// DRAM is shared by every task; without this bound one EL0 task could `mmap`
/// the entire machine and starve the scheduler, IPC and the kernel heap. The
/// frame allocator's global limit is the last line of defence, not the first.
pub fn mmap_quota_ok(current: usize, request: usize, limit: usize) -> bool {
    request != 0 && current.checked_add(request).is_some_and(|total| total <= limit)
}

/// Aligns a half-open physical interval inward to `alignment`.
///
/// The frame allocator must never hand out a partial page, and a malformed
/// live-FDT window must fail closed instead of aborting the kernel. Returns
/// `None` for a zero or non-power-of-two alignment, an overflow while
/// rounding the start, or an interval that contains no whole aligned unit.
pub fn align_range(start: usize, end: usize, alignment: usize) -> Option<(usize, usize)> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return None;
    }
    let mask = alignment - 1;
    let start = start.checked_add(mask)? & !mask;
    let end = end & !mask;
    if end <= start {
        None
    } else {
        Some((start, end))
    }
}

/// Clears `bit` in a frame-bitmap word.
///
/// `None` means the bit was already clear: the caller attempted to free a
/// frame that the allocator believes is not allocated. Treating that as a
/// successful free could hand the same frame to two owners, so the caller
/// must refuse it. (Detecting a double free after the frame was reallocated
/// needs per-frame generation metadata; this helper only classifies the bit.)
pub fn clear_frame_bit(word: u64, bit: u32) -> Option<u64> {
    if bit >= 64 {
        return None;
    }
    let mask = 1_u64 << bit;
    if word & mask == 0 {
        None
    } else {
        Some(word & !mask)
    }
}

/// Component-aware `path` under `base` (or equal to it).
///
/// `/mnt` is *not* under `/mntx`, and `/` contains every absolute path.
/// Both inputs must be normalized absolute paths (as produced by the VFS
/// resolver).
pub fn path_is_under(path: &str, base: &str) -> bool {
    if base == "/" {
        return path.starts_with('/');
    }
    path == base
        || (path.len() > base.len()
            && path.starts_with(base)
            && path.as_bytes()[base.len()] == b'/')
}

/// Longest-prefix mount match for the VFS mount table.
///
/// `mounts` holds every mount point, normalized absolute paths; the root
/// mount (`"/"`) is always present. Returns `(index, relative_path)`, where
/// `relative_path` is the suffix below the matched mount point and is empty
/// when `path` is the mount root itself. Matching is component-aware:
/// `"/mnt"` never matches `"/mntx"`, and the longest matching point wins so
/// nested mounts shadow their parents.
///
/// The returned suffix borrows `path`, not `mounts`, so callers can build the
/// mount-point slice from a temporarily borrowed table.
pub fn mount_match<'m, 'p>(mounts: &[&'m str], path: &'p str) -> Option<(usize, &'p str)> {
    let mut best: Option<(usize, usize)> = None;
    for (index, point) in mounts.iter().enumerate() {
        let matches = if *point == "/" {
            path.starts_with('/')
        } else {
            *point == path
                || (path.len() > point.len()
                    && path.starts_with(*point)
                    && path.as_bytes()[point.len()] == b'/')
        };
        if !matches {
            continue;
        }
        let length = point.len();
        if best.map_or(true, |(_, best_length)| length > best_length) {
            best = Some((index, length));
        }
    }
    best.map(|(index, length)| {
        let relative = &path[length..];
        let relative = relative.strip_prefix('/').unwrap_or(relative);
        (index, relative)
    })
}

/// Capacity of the in-flight DMA lease table.
pub const DMA_LEASE_CAPACITY: usize = 16;

/// Fixed-capacity table of in-flight DMA claims.
///
/// A driver arms a physical range *before* handing the buffer to a device and
/// releases it only after it has observed completion and stopped the queue.
/// The kernel consults this table before freeing a revoked range: a live claim
/// means the device may still issue DMA, so the frames are leaked instead of
/// being reused under a running device.
///
/// The table is pure so the overlap, capacity and slot rules are host-tested;
/// the kernel wraps it in a spinlock and exposes the RAII handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaLeaseTable {
    entries: [Option<(usize, usize)>; DMA_LEASE_CAPACITY],
}

impl DmaLeaseTable {
    pub const fn new() -> Self {
        Self {
            entries: [None; DMA_LEASE_CAPACITY],
        }
    }

    /// Claims `[physical, physical + byte_len)`. Returns a slot token for
    /// [`release`](Self::release). Refuses a zero-length or overflowing range
    /// and a full table.
    pub fn arm(&mut self, physical: usize, byte_len: usize) -> Option<usize> {
        if byte_len == 0 || physical.checked_add(byte_len).is_none() {
            return None;
        }
        let slot = self.entries.iter().position(Option::is_none)?;
        self.entries[slot] = Some((physical, byte_len));
        Some(slot)
    }

    /// Releases the claim in `slot`. A stale slot is ignored: the token is not
    /// a capability and cannot affect another driver's range.
    pub fn release(&mut self, slot: usize) {
        if let Some(entry) = self.entries.get_mut(slot) {
            *entry = None;
        }
    }

    /// True when any live claim overlaps `[physical, physical + byte_len)`.
    /// Fails closed on overflow (returns `true`) so a malformed range cannot
    /// bypass the gate.
    pub fn range_busy(&self, physical: usize, byte_len: usize) -> bool {
        if byte_len == 0 {
            return false;
        }
        let Some(end) = physical.checked_add(byte_len) else {
            return true;
        };
        self.entries.iter().flatten().any(|&(start, len)| {
            let Some(other_end) = start.checked_add(len) else {
                return true;
            };
            start < end && physical < other_end
        })
    }

    pub fn active(&self) -> usize {
        self.entries.iter().filter(|entry| entry.is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        align_range, build_argv_envp, clear_frame_bit, mmap_place, mmap_quota_ok, mount_match,
        parse_spawn_args, path_is_under, DmaLeaseTable, DMA_LEASE_CAPACITY, MAX_SPAWN_ARGS,
        SPAWN_ARGS_VERSION,
    };

    const PAGE: usize = 4096;
    // A scaled-down model of the kernel window (mmu::USER_MMAP_VA ..
    // mmu::USER_MMAP_MAX_VA), aligned the same way.
    const MIN_VA: usize = 0x0040_0000;
    const MAX_VA: usize = 0x4020_0000; // MIN_VA + 1024 MiB
    const MAX_PAGES: usize = (MAX_VA - MIN_VA) / PAGE; // 262144 pages
    const MAX_LENGTH: usize = MAX_PAGES * PAGE; // largest accepted length

    fn regions(list: &[(usize, usize)]) -> impl Iterator<Item = (usize, usize)> + '_ {
        list.iter().copied()
    }

    #[test]
    fn usize_max_is_rejected_without_overflow() {
        // Regression: `page_count * PAGE_SIZE` used to overflow here.
        // In debug builds this panicked (kernel halt); it must simply fail.
        assert_eq!(mmap_place(usize::MAX, MIN_VA, MAX_VA, PAGE, regions(&[])), None);
        // The exact 4095-value overflow window, plus its lower neighbour.
        assert_eq!(mmap_place(usize::MAX - 4095, MIN_VA, MAX_VA, PAGE, regions(&[])), None);
        assert_eq!(mmap_place(usize::MAX - 4096, MIN_VA, MAX_VA, PAGE, regions(&[])), None);
    }

    #[test]
    fn max_allowed_length_fits_exactly() {
        let placed = mmap_place(MAX_LENGTH, MIN_VA, MAX_VA, PAGE, regions(&[]));
        assert_eq!(placed, Some((MIN_VA, MAX_PAGES)));
        assert_eq!(placed.unwrap().0 + MAX_LENGTH, MAX_VA);
    }

    #[test]
    fn one_page_above_the_limit_is_rejected() {
        assert_eq!(
            mmap_place(MAX_LENGTH + PAGE, MIN_VA, MAX_VA, PAGE, regions(&[])),
            None
        );
        // One byte above the limit still rounds up to a page too many.
        assert_eq!(
            mmap_place(MAX_LENGTH + 1, MIN_VA, MAX_VA, PAGE, regions(&[])),
            None
        );
    }

    #[test]
    fn zero_length_is_rejected() {
        assert_eq!(mmap_place(0, MIN_VA, MAX_VA, PAGE, regions(&[])), None);
    }

    #[test]
    fn first_fit_skips_existing_regions() {
        let existing = [(MIN_VA, 4)]; // one region at the window start
        let placed = mmap_place(2 * PAGE, MIN_VA, MAX_VA, PAGE, regions(&existing));
        assert_eq!(placed, Some((MIN_VA + 4 * PAGE, 2)));
    }

    #[test]
    fn no_gap_before_next_region_falls_through() {
        // Region occupies everything the request could want at the start.
        let existing = [(MIN_VA, 8)];
        let placed = mmap_place(8 * PAGE, MIN_VA, MAX_VA, PAGE, regions(&existing));
        assert_eq!(placed, Some((MIN_VA + 8 * PAGE, 8)));
    }

    #[test]
    fn overflowing_region_list_is_rejected_cleanly() {
        // Corrupted kernel-side region data must not panic the helper either.
        let bad = [(usize::MAX - PAGE / 2, usize::MAX)];
        assert_eq!(mmap_place(PAGE, MIN_VA, MAX_VA, PAGE, regions(&bad)), None);
    }

    // --- per-task anonymous-mapping quota ---------------------------------

    const QUOTA: usize = 16 * 1024 * 1024;

    #[test]
    fn quota_allows_exact_limit_and_no_more() {
        assert!(mmap_quota_ok(0, QUOTA, QUOTA));
        assert!(mmap_quota_ok(QUOTA - PAGE, PAGE, QUOTA));
        assert!(!mmap_quota_ok(0, QUOTA + PAGE, QUOTA));
        assert!(!mmap_quota_ok(QUOTA, PAGE, QUOTA));
    }

    #[test]
    fn quota_rejects_zero_and_overflow() {
        assert!(!mmap_quota_ok(0, 0, QUOTA));
        assert!(!mmap_quota_ok(usize::MAX, 1, QUOTA));
        assert!(!mmap_quota_ok(QUOTA, usize::MAX, QUOTA));
    }

    #[test]
    fn quota_repeated_allocations_accumulate() {
        let mut used = 0;
        for _ in 0..4 {
            assert!(mmap_quota_ok(used, QUOTA / 4, QUOTA));
            used += QUOTA / 4;
        }
        assert!(!mmap_quota_ok(used, PAGE, QUOTA));
    }

    // --- mount table resolution -------------------------------------------

    const ROOT_ONLY: &[&str] = &["/"];

    #[test]
    fn root_mount_catches_every_path() {
        assert_eq!(mount_match(ROOT_ONLY, "/"), Some((0, "")));
        assert_eq!(mount_match(ROOT_ONLY, "/etc/passwd"), Some((0, "etc/passwd")));
    }

    #[test]
    fn exact_mount_root_yields_empty_suffix() {
        let mounts = &["/", "/mnt"];
        assert_eq!(mount_match(mounts, "/mnt"), Some((1, "")));
        assert_eq!(mount_match(mounts, "/mnt/file"), Some((1, "file")));
        assert_eq!(mount_match(mounts, "/etc/x"), Some((0, "etc/x")));
    }

    #[test]
    fn longest_prefix_wins_for_nested_mounts() {
        let mounts = &["/", "/mnt", "/mnt/disk", "/mnt/disk/sub"];
        assert_eq!(mount_match(mounts, "/mnt/disk/sub/a/b"), Some((3, "a/b")));
        assert_eq!(mount_match(mounts, "/mnt/disk/x"), Some((2, "x")));
        assert_eq!(mount_match(mounts, "/mnt/x"), Some((1, "x")));
    }

    #[test]
    fn match_is_component_aware() {
        let mounts = &["/", "/mnt"];
        // "/mntx" must fall through to the root, not match "/mnt".
        assert_eq!(mount_match(mounts, "/mntx/y"), Some((0, "mntx/y")));
    }

    #[test]
    fn path_is_under_is_component_aware() {
        assert!(path_is_under("/", "/"));
        assert!(path_is_under("/a/b", "/"));
        assert!(path_is_under("/a/b", "/a"));
        assert!(path_is_under("/a", "/a"));
        assert!(!path_is_under("/ab", "/a"));
        assert!(!path_is_under("/a", "/a/b"));
        assert!(!path_is_under("/mntx", "/mnt"));
    }

    // --- spawn-argument block (SYS_SPAWN_ARGSV) ---------------------------

    fn spawn_block(argv: &[&[u8]], envp: &[&[u8]]) -> Vec<u8> {
        let mut block = vec![0u8; 24];
        let mut data = Vec::new();
        for text in argv {
            data.extend_from_slice(text);
            data.push(0);
        }
        let argv_off = 24;
        let envp_off = argv_off + data.len();
        for text in envp {
            data.extend_from_slice(text);
            data.push(0);
        }
        block.extend_from_slice(&data);
        block[0..4].copy_from_slice(&SPAWN_ARGS_VERSION.to_le_bytes());
        block[4..8].copy_from_slice(&(argv.len() as u32).to_le_bytes());
        block[8..12].copy_from_slice(&(envp.len() as u32).to_le_bytes());
        block[12..16].copy_from_slice(&(argv_off as u32).to_le_bytes());
        block[16..20].copy_from_slice(&(envp_off as u32).to_le_bytes());
        let len = block.len() as u32;
        block[20..24].copy_from_slice(&len.to_le_bytes());
        block
    }

    #[test]
    fn spawn_args_round_trip() {
        let block = spawn_block(&[b"echo", b"hi"], &[b"PATH=/bin"]);
        let args = parse_spawn_args(&block).expect("valid block");
        assert_eq!(args.argv, vec![b"echo".to_vec(), b"hi".to_vec()]);
        assert_eq!(args.envp, vec![b"PATH=/bin".to_vec()]);
    }

    #[test]
    fn spawn_args_rejects_bad_version() {
        let mut block = spawn_block(&[b"a"], &[]);
        block[0..4].copy_from_slice(&99u32.to_le_bytes());
        assert!(parse_spawn_args(&block).is_none());
    }

    #[test]
    fn spawn_args_rejects_total_mismatch() {
        let mut block = spawn_block(&[b"a"], &[]);
        let len = (block.len() + 1) as u32;
        block[20..24].copy_from_slice(&len.to_le_bytes());
        assert!(parse_spawn_args(&block).is_none());
    }

    #[test]
    fn spawn_args_rejects_missing_nul() {
        // argc says one string, but the string table has no terminator.
        let mut block = spawn_block(&[], &[]);
        block.extend_from_slice(b"unterminated");
        block[4..8].copy_from_slice(&1u32.to_le_bytes());
        let len = block.len() as u32;
        block[20..24].copy_from_slice(&len.to_le_bytes());
        assert!(parse_spawn_args(&block).is_none());
    }

    #[test]
    fn spawn_args_rejects_too_many_entries() {
        let mut block = spawn_block(&[], &[]);
        block[4..8].copy_from_slice(&((MAX_SPAWN_ARGS as u32) + 1).to_le_bytes());
        assert!(parse_spawn_args(&block).is_none());
    }

    #[test]
    fn spawn_args_rejects_short_header() {
        assert!(parse_spawn_args(&[0u8; 23]).is_none());
    }

    #[test]
    fn argv_envp_layout_is_consistent() {
        let argv = vec![b"prog".to_vec(), b"arg".to_vec()];
        let envp = vec![b"K=V".to_vec()];
        let top = 0x7FE0_1000usize;
        let layout = build_argv_envp(&argv, &envp, top, 8192).expect("fits");
        assert_eq!(layout.sp % 16, 0);
        assert_eq!(layout.sp + layout.block.len(), top);
        assert_eq!(
            u64::from_le_bytes(layout.block[0..8].try_into().unwrap()),
            2
        );
        // argv_ptrs=8, envp_ptrs=32, strings=48.
        let argv0 = u64::from_le_bytes(layout.block[8..16].try_into().unwrap()) as usize;
        let envp0 = u64::from_le_bytes(layout.block[32..40].try_into().unwrap()) as usize;
        assert_eq!(argv0, layout.sp + 48);
        assert_eq!(envp0, layout.sp + 57);
        assert_eq!(&layout.block[48..53], b"prog\0");
        assert_eq!(&layout.block[53..57], b"arg\0");
        assert_eq!(&layout.block[57..61], b"K=V\0");
        // Both NULL terminators stay zero.
        assert_eq!(&layout.block[24..32], &[0u8; 8]);
        assert_eq!(&layout.block[40..48], &[0u8; 8]);
    }

    #[test]
    fn argv_envp_rejects_embedded_nul() {
        assert!(build_argv_envp(&[b"a\0b".to_vec()], &[], 0x2000, 4096).is_none());
    }

    #[test]
    fn argv_envp_rejects_oversize() {
        let big = vec![b'x'; 4096];
        assert!(build_argv_envp(&[big], &[], 0x2000, 64).is_none());
    }

    #[test]
    fn argv_envp_rejects_too_many_entries() {
        let many: Vec<Vec<u8>> = (0..=MAX_SPAWN_ARGS).map(|_| b"x".to_vec()).collect();
        assert!(build_argv_envp(&many, &[], 0x10000, 8192).is_none());
    }

    #[test]
    fn align_range_rounds_inward() {
        assert_eq!(align_range(0x1003, 0x3fff, 0x1000), Some((0x2000, 0x3000)));
        assert_eq!(align_range(0x1000, 0x2fff, 0x1000), Some((0x1000, 0x2000)));
        assert_eq!(align_range(0x1000, 0x2000, 0x1000), Some((0x1000, 0x2000)));
    }

    #[test]
    fn align_range_rejects_sub_page_and_bad_alignment() {
        assert_eq!(align_range(0x1000, 0x1800, 0x1000), None);
        assert_eq!(align_range(0x1000, 0x1000, 0x1000), None);
        assert_eq!(align_range(0, 0x1000, 0), None);
        assert_eq!(align_range(0, 0x1000, 3), None);
    }

    #[test]
    fn align_range_rejects_overflow() {
        assert_eq!(align_range(usize::MAX - 1, usize::MAX, 0x1000), None);
    }

    #[test]
    fn clear_frame_bit_classifies_double_free() {
        assert_eq!(clear_frame_bit(0b1010, 1), Some(0b1000));
        assert_eq!(clear_frame_bit(0b1010, 0), None);
        assert_eq!(clear_frame_bit(0, 0), None);
        assert_eq!(clear_frame_bit(u64::MAX, 64), None);
    }

    #[test]
    fn dma_lease_reports_overlap_at_both_edges() {
        let mut table = DmaLeaseTable::new();
        let slot = table.arm(0x1000, 0x1000).expect("lease");
        assert!(table.range_busy(0x1000, 1));
        assert!(table.range_busy(0x1FFF, 1));
        assert!(table.range_busy(0x0800, 0x1000));
        assert!(table.range_busy(0x1800, 0x1000));
        assert!(!table.range_busy(0x2000, 0x1000));
        assert!(!table.range_busy(0x0000, 0x1000));
        table.release(slot);
        assert!(!table.range_busy(0x1000, 0x1000));
        assert_eq!(table.active(), 0);
    }

    #[test]
    fn dma_lease_capacity_is_bounded() {
        let mut table = DmaLeaseTable::new();
        for index in 0..DMA_LEASE_CAPACITY {
            assert_eq!(table.arm(0x1000 * (index + 1), 0x1000), Some(index));
        }
        assert_eq!(table.arm(0x1000, 0x1000), None);
        assert_eq!(table.active(), DMA_LEASE_CAPACITY);
        table.release(3);
        assert_eq!(table.arm(0x9000, 0x1000), Some(3));
    }

    #[test]
    fn dma_lease_rejects_bad_ranges_and_stale_slots() {
        let mut table = DmaLeaseTable::new();
        assert_eq!(table.arm(0x1000, 0), None);
        assert_eq!(table.arm(usize::MAX, 1), None);
        // A stale slot release must not affect another live claim.
        let slot = table.arm(0x1000, 0x1000).unwrap();
        table.release(slot + 1);
        assert!(table.range_busy(0x1000, 0x1000));
    }

    #[test]
    fn dma_lease_range_busy_fails_closed_on_overflow() {
        let table = DmaLeaseTable::new();
        assert!(table.range_busy(usize::MAX, 2));
    }
}
