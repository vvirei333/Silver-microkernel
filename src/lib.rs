#![cfg_attr(not(test), no_std)]

//! Shared pure logic of the Silver microkernel.
//!
//! This module is deliberately free of kernel state so the placement
//! arithmetic can be regression-tested on the host (`cargo test --lib`)
//! while the no_std kernel links the very same code.

extern crate alloc;

pub mod fdt;
pub mod gicv3_policy;
pub mod reloc;
pub mod wait_queue;

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

#[cfg(test)]
mod tests {
    use super::{
        build_argv_envp, mmap_place, mount_match, parse_spawn_args, path_is_under, MAX_SPAWN_ARGS,
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
}
