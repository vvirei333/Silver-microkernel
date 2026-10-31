#![cfg_attr(not(test), no_std)]

//! Shared pure logic of the Silver microkernel.
//!
//! This module is deliberately free of kernel state so the placement
//! arithmetic can be regression-tested on the host (`cargo test --lib`)
//! while the no_std kernel links the very same code.

extern crate alloc;

pub mod wait_queue;

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

#[cfg(test)]
mod tests {
    use super::mmap_place;

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
}
