//! Pure physical-range arithmetic shared by the MMU's revoke and mapping
//! paths.
//!
//! These helpers are deliberately free of kernel state so the boundary rules
//! can be regression-tested on the host (`cargo test --lib`): a range must be
//! wholly inside the platform window, page-aligned where the caller promises a
//! page range, and must not overflow `usize`.

/// End of the half-open byte range `[start, start + size)` when it is
/// non-empty, does not overflow, and lies inside `[lower, upper]`.
///
/// `None` is the fail-closed answer for every invalid input, including
/// `size == 0`, a start below `lower`, or an end above `upper`.
pub fn checked_range_end(
    start: usize,
    size: usize,
    lower: usize,
    upper: usize,
) -> Option<usize> {
    if size == 0 {
        return None;
    }
    let end = start.checked_add(size)?;
    if start < lower || end > upper {
        return None;
    }
    Some(end)
}

/// True when `[start, end)` is a page-aligned, non-decreasing range inside
/// `[lower, upper]`. An empty range (`start == end`) is accepted when its
/// single bound is valid and aligned; callers that reject empties do so
/// separately.
pub fn page_range_within(
    start: usize,
    end: usize,
    page_size: usize,
    lower: usize,
    upper: usize,
) -> bool {
    page_size.is_power_of_two()
        && start % page_size == 0
        && end % page_size == 0
        && start <= end
        && start >= lower
        && end <= upper
}

/// Aligns a byte range to whole pages for mapping.
///
/// Returns `(page_start, offset, page_count)` where `page_start` is `start`
/// rounded down to `page_size`, `offset` is the skipped prefix, and
/// `page_count` is the number of pages needed to cover `offset + size`.
/// `None` for `size == 0`, a non-power-of-two page size, or an arithmetic
/// overflow. The caller still has to bound `page_count` for its window.
pub fn aligned_page_span(
    start: usize,
    size: usize,
    page_size: usize,
) -> Option<(usize, usize, usize)> {
    if size == 0 || !page_size.is_power_of_two() {
        return None;
    }
    let page_start = start & !(page_size - 1);
    let offset = start - page_start;
    let mapped_size = offset.checked_add(size)?;
    let page_count = mapped_size.div_ceil(page_size);
    if page_count == 0 {
        return None;
    }
    Some((page_start, offset, page_count))
}

#[cfg(test)]
mod tests {
    use super::{aligned_page_span, checked_range_end, page_range_within};

    const PAGE: usize = 4096;
    const DRAM_BASE: usize = 0x4000_0000;
    const DRAM_END: usize = 0x8000_0000;

    #[test]
    fn checked_range_rejects_below_lower_bound() {
        assert_eq!(
            checked_range_end(DRAM_BASE - 1, 1, DRAM_BASE, DRAM_END),
            None
        );
        assert_eq!(
            checked_range_end(DRAM_BASE - PAGE, PAGE, DRAM_BASE, DRAM_END),
            None
        );
    }

    #[test]
    fn checked_range_accepts_the_last_byte_of_the_window() {
        assert_eq!(
            checked_range_end(DRAM_END - 1, 1, DRAM_BASE, DRAM_END),
            Some(DRAM_END)
        );
        assert_eq!(
            checked_range_end(DRAM_END - PAGE, PAGE, DRAM_BASE, DRAM_END),
            Some(DRAM_END)
        );
        // One byte past the window fails closed.
        assert_eq!(checked_range_end(DRAM_END, 1, DRAM_BASE, DRAM_END), None);
        assert_eq!(
            checked_range_end(DRAM_END - 1, 2, DRAM_BASE, DRAM_END),
            None
        );
    }

    #[test]
    fn checked_range_rejects_zero_and_overflow() {
        assert_eq!(checked_range_end(DRAM_BASE, 0, DRAM_BASE, DRAM_END), None);
        assert_eq!(
            checked_range_end(usize::MAX, 2, DRAM_BASE, usize::MAX),
            None
        );
    }

    #[test]
    fn page_range_rejects_a_base_below_dram() {
        assert!(!page_range_within(
            DRAM_BASE - PAGE,
            DRAM_BASE,
            PAGE,
            DRAM_BASE,
            DRAM_END
        ));
        assert!(page_range_within(
            DRAM_BASE,
            DRAM_BASE + PAGE,
            PAGE,
            DRAM_BASE,
            DRAM_END
        ));
    }

    #[test]
    fn page_range_rejects_unaligned_and_reversed_bounds() {
        assert!(!page_range_within(
            DRAM_BASE + 1,
            DRAM_BASE + PAGE,
            PAGE,
            DRAM_BASE,
            DRAM_END
        ));
        assert!(!page_range_within(
            DRAM_BASE,
            DRAM_BASE + PAGE + 1,
            PAGE,
            DRAM_BASE,
            DRAM_END
        ));
        assert!(!page_range_within(
            DRAM_BASE + PAGE,
            DRAM_BASE,
            PAGE,
            DRAM_BASE,
            DRAM_END
        ));
    }

    #[test]
    fn page_range_accepts_an_empty_range_at_the_bounds() {
        assert!(page_range_within(
            DRAM_END,
            DRAM_END,
            PAGE,
            DRAM_BASE,
            DRAM_END
        ));
        assert!(!page_range_within(
            DRAM_END + PAGE,
            DRAM_END + PAGE,
            PAGE,
            DRAM_BASE,
            DRAM_END
        ));
    }

    #[test]
    fn aligned_span_rounds_a_one_byte_range_at_dram_end() {
        assert_eq!(
            aligned_page_span(DRAM_END - 1, 1, PAGE),
            Some((DRAM_END - PAGE, PAGE - 1, 1))
        );
    }

    #[test]
    fn aligned_span_rounds_up_across_pages() {
        assert_eq!(
            aligned_page_span(DRAM_BASE + 1, PAGE, PAGE),
            Some((DRAM_BASE, 1, 2))
        );
        assert_eq!(
            aligned_page_span(DRAM_BASE, PAGE, PAGE),
            Some((DRAM_BASE, 0, 1))
        );
    }

    #[test]
    fn aligned_span_rejects_zero_size_and_overflow() {
        assert_eq!(aligned_page_span(DRAM_BASE, 0, PAGE), None);
        assert_eq!(aligned_page_span(usize::MAX, usize::MAX, PAGE), None);
    }
}
