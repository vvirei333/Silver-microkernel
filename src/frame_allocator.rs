use core::sync::atomic::{AtomicUsize, Ordering};

use crate::memory::{MemoryRegion, PAGE_SIZE};
use crate::smp::SpinLock;

pub const MAX_PHYSICAL_PAGES: usize = crate::memory::DRAM_SIZE / PAGE_SIZE;
const BITS_PER_WORD: usize = 64;
const BITMAP_WORDS: usize = MAX_PHYSICAL_PAGES / BITS_PER_WORD;
const ALL_PAGES_USED: u64 = u64::MAX;

struct FrameAllocatorState {
    bitmap: [u64; BITMAP_WORDS],
    /// One tag per frame, rewritten on every allocation. A free must present
    /// the tag it received from that allocation; a stale free after the frame
    /// was recycled fails instead of returning a live frame.
    generation: [u32; MAX_PHYSICAL_PAGES],
    /// Owners of a live frame. A fresh allocation is 1; `fork` bumps it for
    /// copy-on-write and a release only decrements until the last owner.
    refcount: [u16; MAX_PHYSICAL_PAGES],
    next_generation: u32,
    available: MemoryRegion,
    total_pages: usize,
    free_pages: usize,
}

/// The allocator state lives behind a spinlock: two CPUs racing in
/// `alloc_page` would otherwise both claim the same free bit. All mutators
/// (and the read-only queries used by revoke validation) take the lock;
/// every call site runs with the kernel lock held, so the spin here is
/// bounded by another CPU's short critical section.
static STATE: SpinLock<FrameAllocatorState> = SpinLock::new(FrameAllocatorState {
    bitmap: [0; BITMAP_WORDS],
    generation: [0; MAX_PHYSICAL_PAGES],
    refcount: [0; MAX_PHYSICAL_PAGES],
    next_generation: 1,
    available: MemoryRegion::new(0, 0),
    total_pages: 0,
    free_pages: 0,
});

/// Release-mode counter for refused stale/double frees. The page is already
/// free or belongs to a newer allocation, so returning it instead of counting
/// could hand a live frame to a second owner; the counter keeps the event
/// observable without corrupting state.
static STALE_FREES: AtomicUsize = AtomicUsize::new(0);

#[allow(dead_code)]
pub fn double_frees() -> usize {
    STALE_FREES.load(Ordering::Relaxed)
}

/// Initializes the allocator from page-aligned part of the available memory region.
///
/// Returns the number of usable physical pages. The bitmap itself lives in the
/// kernel's `.bss`, so it does not consume frames from `available`.
pub fn init(available: MemoryRegion) -> usize {
    // A malformed or truncated live-FDT window must fail closed, not abort:
    // the caller already turns a zero return into an exception-6 boot stop.
    let Some((start, end)) =
        silver_microkernel::align_range(available.start, available.end, PAGE_SIZE)
    else {
        return 0;
    };
    let requested_pages = (end - start) / PAGE_SIZE;
    let total_pages = requested_pages.min(MAX_PHYSICAL_PAGES);
    let usable_end = start + total_pages * PAGE_SIZE;

    let mut state = STATE.lock();
    state.bitmap.fill(ALL_PAGES_USED);
    state.generation.fill(0);
    state.refcount.fill(0);
    state.next_generation = 1;
    state.available = MemoryRegion::new(start, usable_end);
    state.total_pages = total_pages;
    state.free_pages = total_pages;

    for page_index in 0..total_pages {
        mark_free(&mut state, page_index);
    }

    total_pages
}

pub fn total_pages() -> usize {
    STATE.lock().total_pages
}

pub fn free_pages() -> usize {
    STATE.lock().free_pages
}

pub fn used_pages() -> usize {
    let state = STATE.lock();
    state.total_pages - state.free_pages
}

/// Test/bisect lever: a value other than `usize::MAX` replaces the policy
/// reserve. The boot self-test uses it to prove the gate refuses without
/// draining the machine.
static USER_RESERVE_OVERRIDE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Frames currently kept out of user-visible allocations.
pub fn user_reserve_frames() -> usize {
    let override_value = USER_RESERVE_OVERRIDE.load(Ordering::Relaxed);
    if override_value == usize::MAX {
        silver_microkernel::mem_policy::reserve_frames(total_pages())
    } else {
        override_value
    }
}

fn user_alloc_allowed(page_count: usize) -> bool {
    let free = free_pages();
    let override_value = USER_RESERVE_OVERRIDE.load(Ordering::Relaxed);
    if override_value == usize::MAX {
        silver_microkernel::mem_policy::user_alloc_ok(free, page_count, total_pages())
    } else {
        free.checked_sub(override_value)
            .map_or(false, |remaining| remaining >= page_count)
    }
}

/// Allocates one frame for user-visible memory. Refuses when the request would
/// dip into [`user_reserve_frames`], so user tasks cannot drain the frames the
/// kernel and its devices still need. Every caller runs under the kernel lock,
/// which serializes the check with the other CPUs' user allocations.
pub fn alloc_user_page_tagged() -> Option<(usize, u32)> {
    if !user_alloc_allowed(1) {
        return None;
    }
    alloc_page_tagged()
}

/// Contiguous variant of [`alloc_user_page_tagged`].
pub fn alloc_user_contiguous_pages_tagged(page_count: usize) -> Option<(usize, u32)> {
    if page_count == 0 || !user_alloc_allowed(page_count) {
        return None;
    }
    alloc_contiguous_pages_tagged(page_count)
}

/// Boot proof that the user-reserve gate is wired and one-sided: it refuses a
/// user page when the reserve is forced one frame above the free count, while
/// the raw kernel allocator keeps working and the normal path opens again once
/// the override is cleared.
pub fn run_user_reserve_selftest() -> bool {
    let free = free_pages();
    let total = total_pages();
    let reserve = silver_microkernel::mem_policy::reserve_frames(total);
    let policy_ok = reserve <= total / 2 && reserve < total.max(1);

    // The normal path must be grantable on a healthy boot.
    let normal = alloc_user_page_tagged();
    let normal_ok = normal.is_some();
    if let Some((page, tag)) = normal {
        free_page_tagged(page, tag);
    }

    // Force the reserve above the free count: user allocations are refused,
    // kernel allocations are not.
    USER_RESERVE_OVERRIDE.store(free.saturating_add(1), Ordering::Relaxed);
    let refused = alloc_user_page_tagged().is_none();
    let kernel = alloc_page_tagged();
    let kernel_ok = kernel.is_some();
    if let Some((page, tag)) = kernel {
        free_page_tagged(page, tag);
    }

    // Clearing the override must restore the policy, not leave it closed.
    USER_RESERVE_OVERRIDE.store(usize::MAX, Ordering::Relaxed);
    let restored = alloc_user_page_tagged();
    let restored_ok = restored.is_some();
    if let Some((page, tag)) = restored {
        free_page_tagged(page, tag);
    }

    let ok = policy_ok && normal_ok && refused && kernel_ok && restored_ok;
    // The tablet has no proven console; keep the check, skip the writes there.
    #[cfg(not(feature = "tablet"))]
    if ok {
        crate::console::write_str("MEM RESERVE PASS (free=");
        crate::console::write_dec_u64(free as u64);
        crate::console::write_str(" reserve=");
        crate::console::write_dec_u64(reserve as u64);
        crate::console::write_str(")\r\n");
    }
    #[cfg(not(feature = "tablet"))]
    if !ok {
        crate::console::write_str("MEM RESERVE FAIL (free=");
        crate::console::write_dec_u64(free as u64);
        crate::console::write_str(" reserve=");
        crate::console::write_dec_u64(reserve as u64);
        crate::console::write_str(" normal=");
        crate::console::write_dec_u64(u64::from(normal_ok));
        crate::console::write_str(" refused=");
        crate::console::write_dec_u64(u64::from(refused));
        crate::console::write_str(" kernel=");
        crate::console::write_dec_u64(u64::from(kernel_ok));
        crate::console::write_str(" restored=");
        crate::console::write_dec_u64(u64::from(restored_ok));
        crate::console::write_str(")\r\n");
    }
    ok
}

/// Read-only allocation query: `true` only when `address` is a page-aligned
/// frame inside the managed range whose bitmap bit is currently set (used).
///
/// The allocator tracks allocation, not purpose: a `true` result proves the
/// frame is live, but not what it holds (it could be data or a page table).
/// A `false` result proves the address is not a live kernel allocation —
/// exactly what a validation path needs to reject a descriptor that names
/// unmanaged, freed, or unaligned memory.
pub fn frame_in_use(address: usize) -> bool {
    let state = STATE.lock();
    if address % PAGE_SIZE != 0 || !state.available.contains(address) {
        return false;
    }

    let page_index = (address - state.available.start) / PAGE_SIZE;
    if page_index >= state.total_pages {
        return false;
    }

    state.bitmap[page_index / BITS_PER_WORD] & (1_u64 << (page_index % BITS_PER_WORD)) != 0
}

/// Allocates one physical page and returns `(address, generation)`.
pub fn alloc_page_tagged() -> Option<(usize, u32)> {
    let mut state = STATE.lock();
    let word_count = state.total_pages.div_ceil(BITS_PER_WORD);

    for word_index in 0..word_count {
        let free_mask = !state.bitmap[word_index];
        if free_mask == 0 {
            continue;
        }

        let bit_index = free_mask.trailing_zeros() as usize;
        let page_index = word_index * BITS_PER_WORD + bit_index;
        if page_index >= state.total_pages {
            return None;
        }

        let Some(tag) = next_generation(&mut state) else {
            return None;
        };
        mark_used(&mut state, page_index);
        state.generation[page_index] = tag;
        state.refcount[page_index] = 1;
        state.free_pages -= 1;
        return Some((state.available.start + page_index * PAGE_SIZE, tag));
    }

    None
}

/// Allocates a run of `page_count` physically contiguous pages.
///
/// Searches the bitmap for the lowest run of `page_count` consecutive free bits
/// and claims the whole run at once. Fragmented memory is never returned: DMA
/// consumers (VirtIO rings) need one physically contiguous buffer, so a missing
/// run is reported as `None` instead of being spread across the bitmap.
///
/// The state is borrowed exactly once per call. The helpers used below take a
/// `&mut FrameAllocatorState` instead of going back through the public
/// allocation/free entry points, which would borrow the same state twice.
pub fn alloc_contiguous_pages_tagged(page_count: usize) -> Option<(usize, u32)> {
    if page_count == 0 || page_count > MAX_PHYSICAL_PAGES {
        return None;
    }

    let mut state = STATE.lock();
    if page_count > state.free_pages {
        return None;
    }

    let mut run_start = 0_usize;
    let mut run_length = 0_usize;

    for page_index in 0..state.total_pages {
        let word = state.bitmap[page_index / BITS_PER_WORD];
        let is_free = word & (1_u64 << (page_index % BITS_PER_WORD)) == 0;

        if !is_free {
            run_length = 0;
            continue;
        }

        if run_length == 0 {
            run_start = page_index;
        }
        run_length += 1;

        if run_length == page_count {
            let Some(tag) = next_generation(&mut state) else {
                return None;
            };
            for index in run_start..run_start + page_count {
                mark_used(&mut state, index);
                state.generation[index] = tag;
                state.refcount[index] = 1;
            }
            state.free_pages -= page_count;
            return Some((state.available.start + run_start * PAGE_SIZE, tag));
        }
    }

    None
}

/// Contiguous allocation whose tag is discarded. Only for runs that are never
/// freed (the kernel heap and the permanent stack arena).
pub fn alloc_contiguous_pages(page_count: usize) -> Option<usize> {
    alloc_contiguous_pages_tagged(page_count).map(|(address, _)| address)
}

/// Allocates a run of `page_count` free pages whose physical base is aligned
/// to `alignment_pages` pages.
///
/// The kernel stack arena needs one 2 MiB-aligned block so it can replace a
/// single TTBR1 block descriptor with a 4 KiB table. Scanning the bitmap
/// directly keeps the ordinary [`alloc_contiguous_pages`] fast path
/// unchanged.
pub fn alloc_contiguous_pages_aligned_tagged(
    page_count: usize,
    alignment_pages: usize,
) -> Option<(usize, u32)> {
    if page_count == 0
        || alignment_pages == 0
        || !alignment_pages.is_power_of_two()
        || page_count > MAX_PHYSICAL_PAGES
    {
        return None;
    }

    let mut state = STATE.lock();
    if page_count > state.free_pages {
        return None;
    }

    let alignment_bytes = alignment_pages * PAGE_SIZE;
    let last_start = state.total_pages.checked_sub(page_count)?;
    for run_start in 0..=last_start {
        let address = state.available.start + run_start * PAGE_SIZE;
        if address % alignment_bytes != 0 {
            continue;
        }

        let mut free = true;
        for index in run_start..run_start + page_count {
            let word = state.bitmap[index / BITS_PER_WORD];
            if word & (1_u64 << (index % BITS_PER_WORD)) != 0 {
                free = false;
                break;
            }
        }
        if !free {
            continue;
        }

        let Some(tag) = next_generation(&mut state) else {
            return None;
        };
        for index in run_start..run_start + page_count {
            mark_used(&mut state, index);
            state.generation[index] = tag;
            state.refcount[index] = 1;
        }
        state.free_pages -= page_count;
        return Some((address, tag));
    }

    None
}

/// Returns a previously allocated page to the allocator, but only when
/// `generation` matches the tag handed out by the allocation that owns it.
///
/// A stale free after the frame was recycled fails without touching the
/// bitmap: the page may already belong to another owner. `false` is a
/// kernel bug and must be treated as such by the caller.
pub fn free_page_tagged(address: usize, generation: u32) -> bool {
    free_page_tagged_inner(address, generation, true)
}

/// `diagnose = false` is used only by the boot self-test for its intentional
/// stale free; production callers always take the diagnostic path.
fn free_page_tagged_inner(address: usize, generation: u32, diagnose: bool) -> bool {
    let mut state = STATE.lock();
    if address % PAGE_SIZE != 0 || !state.available.contains(address) {
        if diagnose {
            STALE_FREES.fetch_add(1, Ordering::Relaxed);
            debug_assert!(false, "free of an invalid physical page");
        }
        return false;
    }

    let page_index = (address - state.available.start) / PAGE_SIZE;
    if page_index >= state.total_pages {
        if diagnose {
            STALE_FREES.fetch_add(1, Ordering::Relaxed);
            debug_assert!(false, "free outside the managed page range");
        }
        return false;
    }

    let word_index = page_index / BITS_PER_WORD;
    let bit_index = page_index % BITS_PER_WORD;
    let mask = 1_u64 << bit_index;
    if state.bitmap[word_index] & mask == 0 || state.generation[page_index] != generation {
        // Already free, or reallocated since the caller's tag was issued.
        if diagnose {
            STALE_FREES.fetch_add(1, Ordering::Relaxed);
            debug_assert!(false, "stale or double free of a physical page");
        }
        return false;
    }

    if state.refcount[page_index] > 1 {
        state.refcount[page_index] -= 1;
        return true;
    }
    state.bitmap[word_index] &= !mask;
    state.generation[page_index] = 0;
    state.refcount[page_index] = 0;
    state.free_pages += 1;
    true
}

/// Releases a contiguous run only when every page still carries the tag from
/// the owning allocation. Checks the whole run before modifying anything, so
/// a partially stale run is left entirely untouched.
pub fn free_contiguous_pages_tagged(start: usize, page_count: usize, generation: u32) -> bool {
    if page_count == 0 {
        STALE_FREES.fetch_add(1, Ordering::Relaxed);
        debug_assert!(false, "free of an empty page run");
        return false;
    }
    let mut state = STATE.lock();
    if start % PAGE_SIZE != 0 || !state.available.contains(start) {
        STALE_FREES.fetch_add(1, Ordering::Relaxed);
        debug_assert!(false, "free of an invalid page run");
        return false;
    }
    let first = (start - state.available.start) / PAGE_SIZE;
    let Some(last) = first.checked_add(page_count) else {
        STALE_FREES.fetch_add(1, Ordering::Relaxed);
        debug_assert!(false, "free of an overflowing page run");
        return false;
    };
    if last > state.total_pages {
        STALE_FREES.fetch_add(1, Ordering::Relaxed);
        debug_assert!(false, "free run outside the managed page range");
        return false;
    }

    for page_index in first..last {
        let mask = 1_u64 << (page_index % BITS_PER_WORD);
        if state.bitmap[page_index / BITS_PER_WORD] & mask == 0
            || state.generation[page_index] != generation
        {
            STALE_FREES.fetch_add(page_count, Ordering::Relaxed);
            debug_assert!(false, "stale or double free of a page run");
            return false;
        }
    }

    for page_index in first..last {
        if state.refcount[page_index] > 1 {
            state.refcount[page_index] -= 1;
            continue;
        }
        state.bitmap[page_index / BITS_PER_WORD] &= !(1_u64 << (page_index % BITS_PER_WORD));
        state.generation[page_index] = 0;
        state.refcount[page_index] = 0;
        state.free_pages += 1;
    }
    true
}

/// Adds one owner to a live frame for copy-on-write. Fails when the frame is
/// free, its generation is stale, or the refcount is saturated.
pub fn share_page_tagged(address: usize, generation: u32) -> bool {
    let mut state = STATE.lock();
    if address % PAGE_SIZE != 0 || !state.available.contains(address) {
        return false;
    }
    let page_index = (address - state.available.start) / PAGE_SIZE;
    if page_index >= state.total_pages
        || state.bitmap[page_index / BITS_PER_WORD] & (1_u64 << (page_index % BITS_PER_WORD)) == 0
        || state.generation[page_index] != generation
        || state.refcount[page_index] == u16::MAX
    {
        return false;
    }
    state.refcount[page_index] += 1;
    true
}

/// Contiguous variant of [`share_page_tagged`]. Validates the whole run first,
/// so a stale generation never leaves half the run shared.
pub fn share_contiguous_pages_tagged(start: usize, page_count: usize, generation: u32) -> bool {
    if page_count == 0 || start % PAGE_SIZE != 0 {
        return false;
    }
    let mut state = STATE.lock();
    if !state.available.contains(start) {
        return false;
    }
    let first = (start - state.available.start) / PAGE_SIZE;
    let Some(last) = first.checked_add(page_count) else {
        return false;
    };
    if last > state.total_pages {
        return false;
    }
    for page_index in first..last {
        if state.bitmap[page_index / BITS_PER_WORD] & (1_u64 << (page_index % BITS_PER_WORD)) == 0
            || state.generation[page_index] != generation
            || state.refcount[page_index] == u16::MAX
        {
            return false;
        }
    }
    for page_index in first..last {
        state.refcount[page_index] += 1;
    }
    true
}

/// Current tag for a live frame, for diagnostics and the allocator self-test.
pub fn frame_generation(address: usize) -> Option<u32> {
    let state = STATE.lock();
    if address % PAGE_SIZE != 0 || !state.available.contains(address) {
        return None;
    }
    let page_index = (address - state.available.start) / PAGE_SIZE;
    if page_index >= state.total_pages
        || state.bitmap[page_index / BITS_PER_WORD] & (1_u64 << (page_index % BITS_PER_WORD)) == 0
    {
        return None;
    }
    Some(state.generation[page_index])
}

/// Verifies allocation, contiguity, reuse and stale-free refusal without
/// changing the final state.
pub fn run_self_test() -> bool {
    let Some((first, first_tag)) = alloc_page_tagged() else {
        return false;
    };
    let Some((second, second_tag)) = alloc_page_tagged() else {
        free_page_tagged(first, first_tag);
        return false;
    };

    let sequential = second == first + PAGE_SIZE;

    if !free_page_tagged(first, first_tag) {
        free_page_tagged(second, second_tag);
        return false;
    }
    let Some((reused, reused_tag)) = alloc_page_tagged() else {
        free_page_tagged(second, second_tag);
        return false;
    };
    let reuse_ok = reused == first;

    // The old tag belongs to a frame that may now be reused. It must be
    // refused and must not disturb the new owner.
    let stale_refused = !free_page_tagged_inner(first, first_tag, false);
    if !stale_refused {
        free_page_tagged(reused, reused_tag);
        free_page_tagged(second, second_tag);
        return false;
    }

    if reused != second {
        free_page_tagged(second, second_tag);
    }
    free_page_tagged(reused, reused_tag);

    sequential && reuse_ok && stale_refused && run_contiguous_self_test() && run_aligned_self_test()
}

/// Allocates a short run with a 4-page alignment, checks the base, and
/// releases it. This is the same search the kernel stack arena uses.
fn run_aligned_self_test() -> bool {
    const PROBE_PAGES: usize = 4;
    const ALIGNMENT_PAGES: usize = 4;

    let Some((first, generation)) =
        alloc_contiguous_pages_aligned_tagged(PROBE_PAGES, ALIGNMENT_PAGES)
    else {
        return false;
    };
    if first % (ALIGNMENT_PAGES * PAGE_SIZE) != 0 {
        free_contiguous_pages_tagged(first, PROBE_PAGES, generation);
        return false;
    }
    free_contiguous_pages_tagged(first, PROBE_PAGES, generation)
}

/// Allocates the same short run twice. The first run is released before the
/// second attempt, so the bitmap is back to its previous contents and the scan
/// has to return the identical base address.
fn run_contiguous_self_test() -> bool {
    const PROBE_PAGES: usize = 3;

    let Some((first, first_tag)) = alloc_contiguous_pages_tagged(PROBE_PAGES) else {
        return false;
    };
    if !free_contiguous_pages_tagged(first, PROBE_PAGES, first_tag) {
        return false;
    }

    let Some((second, second_tag)) = alloc_contiguous_pages_tagged(PROBE_PAGES) else {
        return false;
    };
    free_contiguous_pages_tagged(second, PROBE_PAGES, second_tag)
        && first % PAGE_SIZE == 0
        && first == second
}

/// Advances the global tag. At `u32::MAX` allocation fails closed instead of
/// wrapping to an old tag and reopening the ABA window. The limit is 2^32-1
/// successful allocations; exhausting it stops new frame allocations rather
/// than silently reusing a generation.
fn next_generation(state: &mut FrameAllocatorState) -> Option<u32> {
    if state.next_generation == u32::MAX {
        return None;
    }
    state.next_generation += 1;
    Some(state.next_generation)
}

fn mark_used(state: &mut FrameAllocatorState, page_index: usize) {
    let word_index = page_index / BITS_PER_WORD;
    let bit_index = page_index % BITS_PER_WORD;
    state.bitmap[word_index] |= 1_u64 << bit_index;
}

fn mark_free(state: &mut FrameAllocatorState, page_index: usize) {
    let word_index = page_index / BITS_PER_WORD;
    let bit_index = page_index % BITS_PER_WORD;
    state.bitmap[word_index] &= !(1_u64 << bit_index);
}
