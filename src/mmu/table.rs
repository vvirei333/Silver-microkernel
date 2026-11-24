use super::*;
use super::kernel::*;

/// Registers a freshly created table frame with the generation tag from its
/// allocation and the owning space root. Returns `None` — leaving the frame
/// unregistered — on duplicate registration (kernel bug) or when the registry
/// cannot grow (out of memory). The caller must then not link the frame into
/// any table and release it with the tagged allocation handle.
pub(super) fn page_table_register(frame: usize, generation: u32, owner: usize) -> Option<TableToken> {
    let mut frames = PAGE_TABLE_FRAMES.lock();
    match frames.register(frame, generation, owner) {
        Some(token) => Some(token),
        None => {
            crate::console::write_str(
                "PAGE TABLE REGISTRY: duplicate frame, invalid token, or out of memory; frame 0x",
            );
            crate::console::write_hex_u64(frame as u64);
            crate::console::write_str(" not registered\r\n");
            None
        }
    }
}

/// True only when `frame` is currently registered as a page table owned by
/// `owner`. Read-only, so it can validate an entire walk before the first
/// table mutation.
pub(super) fn page_table_owned_by(frame: usize, owner: usize) -> bool {
    PAGE_TABLE_FRAMES.lock().owner(frame) == Some(owner)
}

/// A space root is a live frame whose registry owner is itself: that is the
/// same invariant `retire_space` checks, so a walker cannot treat another
/// space's child table as a root.
pub(super) fn space_root_valid(ttbr0: usize) -> bool {
    let root = root_of(ttbr0);
    root % GRANULE == 0
        && frame_allocator::frame_in_use(root)
        && page_table_owned_by(root, root)
}

/// Frees a page-table frame only when `token` is the live registration.
///
/// Fail-closed on every refusal: a stale token, an already-retired frame or a
/// mis-owned record leaves the frame allocated and registered, so a leaked
/// frame can never be reallocated as data while a table still references it.
pub(super) fn free_table_frame(token: TableToken) -> bool {
    let result = PAGE_TABLE_FRAMES.lock().retire(token);
    match result {
        RetireResult::Retired { generation } => {
            if frame_allocator::free_page_tagged(token.frame(), generation) {
                true
            } else {
                crate::console::write_str("PAGE TABLE REGISTRY: allocator refused frame 0x");
                crate::console::write_hex_u64(token.frame() as u64);
                crate::console::write_str(" - leaked\r\n");
                false
            }
        }
        RetireResult::NotRegistered => {
            crate::console::write_str("PAGE TABLE REGISTRY: frame 0x");
            crate::console::write_hex_u64(token.frame() as u64);
            crate::console::write_str(" is not registered - leaked, not freed\r\n");
            false
        }
        RetireResult::StaleToken => {
            crate::console::write_str("PAGE TABLE REGISTRY: stale token for frame 0x");
            crate::console::write_hex_u64(token.frame() as u64);
            crate::console::write_str(" - leaked, not freed\r\n");
            false
        }
        RetireResult::WrongOwner => {
            crate::console::write_str("PAGE TABLE REGISTRY: owner mismatch for frame 0x");
            crate::console::write_hex_u64(token.frame() as u64);
            crate::console::write_str(" - leaked, not freed\r\n");
            false
        }
    }
}

/// Frees a table frame reached by walking `owner`'s page tables. The owner
/// check refuses a frame that a different (recycled) address space owns.
pub(super) fn free_table_frame_owned(frame: usize, owner: usize) -> bool {
    let result = PAGE_TABLE_FRAMES.lock().retire_owned(frame, owner);
    match result {
        RetireResult::Retired { generation } => {
            frame_allocator::free_page_tagged(frame, generation)
        }
        RetireResult::NotRegistered => {
            crate::console::write_str("PAGE TABLE REGISTRY: walked frame 0x");
            crate::console::write_hex_u64(frame as u64);
            crate::console::write_str(" is not registered - leaked\r\n");
            false
        }
        RetireResult::WrongOwner => {
            crate::console::write_str("PAGE TABLE REGISTRY: walked frame 0x");
            crate::console::write_hex_u64(frame as u64);
            crate::console::write_str(" belongs to another space - leaked\r\n");
            false
        }
        RetireResult::StaleToken => false,
    }
}

/// Root physical address of a TTBR0 value (ASID bits removed). Every place
/// that treats a `ttbr0` argument as a frame address must go through this.
pub fn root_of(ttbr0: usize) -> usize {
    ttbr0 & ADDRESS_MASK as usize
}

/// ASID carried by a tagged TTBR0 value.
pub fn asid_of(ttbr0: usize) -> u16 {
    ((ttbr0 >> TTBR0_ASID_SHIFT) & 0xFF) as u16
}

pub(super) fn tagged_ttbr0(root: usize, asid: u16) -> usize {
    root_of(root) | ((asid as usize) << TTBR0_ASID_SHIFT)
}

pub(super) fn alloc_asid() -> Option<u16> {
    let asid = {
        let mut pool = ASIDS.lock();
        pool.live[0] |= 1; // reserve ASID 0 for the boot/kernel TTBR0
        let mut found = None;
        for word in 0..ASID_WORDS {
            let free = !pool.live[word];
            if free != 0 {
                let bit = free.trailing_zeros() as usize;
                pool.live[word] |= 1u64 << bit;
                let candidate = (word * 64 + bit) as u16;
                if candidate != 0 {
                    found = Some(candidate);
                    break;
                }
            }
        }
        found
    }?;
    // A released ASID can still have cached translations from the address
    // space that used it before. Every task maps the same user VAs (image,
    // heap, stack), so a stale entry would alias the new space into a frame
    // it does not own. Invalidate the ASID on every PE before first use.
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack),
        );
    }
    Some(asid)
}

pub(super) fn release_asid(asid: u16) {
    if asid == 0 {
        return;
    }
    let index = asid as usize;
    let mut pool = ASIDS.lock();
    pool.live[index / 64] &= !(1u64 << (index % 64));
}

/// Broadcast-invalidates every translation tagged with `asid`. The `is`
/// suffix covers the inner-shareable domain, so a stale entry on another CPU
/// is dropped too; the trailing `dsb ish` makes the invalidation complete
/// before the caller frees a table or reuses the ASID.
pub(super) fn invalidate_asid(asid: u16) {
    if asid == 0 {
        return;
    }
    unsafe {
        core::arch::asm!(
            "tlbi aside1is, {op}",
            "dsb ish",
            "isb",
            op = in(reg) (asid as u64) << TTBR0_ASID_SHIFT,
            options(nostack),
        );
    }
}

/// Completes a batch of edits to the space named by `ttbr0`: one store-order
/// barrier, one ASID-scoped broadcast invalidation, one completion barrier.
/// ASID 0 (boot tables) falls back to the full broadcast.
pub(super) fn finish_table_changes_for(ttbr0: usize) {
    let asid = asid_of(ttbr0);
    if asid == 0 {
        finish_table_changes();
        return;
    }
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi aside1is, {op}",
            "dsb ish",
            "isb",
            op = in(reg) (asid as u64) << TTBR0_ASID_SHIFT,
            options(nostack),
        );
    }
}

pub(super) fn table_descriptor(table: &Table) -> u64 {
    (virt_to_phys(table as *const Table as usize) as u64) | DESC_TABLE
}

pub(super) fn block_descriptor(output_address: usize, attr_index: u64, shareability: u64) -> u64 {
    ((output_address as u64) & !(BLOCK_2M as u64 - 1))
        | DESC_BLOCK
        | (attr_index << 2)
        | AP_EL1_RW
        | shareability
        | AF
        | UXN
}

/// A 4 KiB kernel page: cacheable, EL1 RW, non-executable. Used for the
/// dedicated kernel stack arena after its 2 MiB block is split.
pub(super) fn kernel_page_descriptor(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
        | AP_EL1_RW
        | SH_INNER
        | AF
        | UXN
        | PXN
}

pub(super) fn mmu_fail_record(page: usize, arm: u64, extra: u64) {
    let index = MMU_FAIL_INDEX.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let task = crate::scheduler::current_task_id();
    unsafe {
        let ring = core::ptr::addr_of_mut!(MMU_FAIL);
        (*ring)[index % MMU_FAIL_LEN] = [page as u64, arm, extra, task];
    }
}

pub(super) fn alloc_registered_table(root: usize) -> Option<usize> {
    let (page, generation) = frame_allocator::alloc_page_tagged()?;
    unsafe {
        zero_page(page);
    }
    if page_table_register(page, generation, root).is_none() {
        frame_allocator::free_page_tagged(page, generation);
        return None;
    }
    Some(page)
}

pub(super) fn table_empty(table: usize) -> bool {
    unsafe {
        let entries = phys_to_virt(table) as *const u64;
        for index in 0..ENTRIES {
            if core::ptr::read_volatile(entries.add(index)) != 0 {
                return false;
            }
        }
        true
    }
}

/// Validates a descriptor that names the next-level page table before the
/// table frame is dereferenced, and returns the table's physical address.
///
/// A `DESC_TABLE` bit alone proves nothing about the safety of the address:
/// corrupted memory can name any location, and `read_entry` through it would
/// be a physical read of arbitrary memory. The address must therefore be a
/// page-aligned, currently allocated frame **and a member of the page-table
/// registry owned by `expected_owner`** (see [`PAGE_TABLE_FRAMES`]) — i.e. a
/// frame this kernel itself created as a table for the space being walked.
/// Contents-based validation is impossible: data can mimic a valid table
/// chain, so membership and ownership are checked, not inferred. Foreign
/// ownership is a hard refusal: an address space that points at another
/// space's table must never be modified, even if that table is otherwise
/// valid.
///
/// `None` means the address cannot be trusted: the caller must report the
/// address space as unverifiable (`safe_to_free = false`) without reading it.
pub(super) fn follow_validated_table(descriptor: u64, expected_owner: usize) -> Option<usize> {
    if descriptor & DESC_TABLE != DESC_TABLE {
        return None;
    }
    let physical = descriptor_address(descriptor) as usize;
    if physical % GRANULE != 0
        || !frame_allocator::frame_in_use(physical)
        || !page_table_owned_by(physical, expected_owner)
    {
        return None;
    }
    Some(physical)
}

/// A table descriptor's address must be a real DRAM frame before a diagnostic
/// walk follows it: a corrupted entry (table bits set, address bits near
/// zero) would otherwise make the *fault dump* itself fault on the direct
/// map and panic the kernel instead of reporting the user fault.
pub(super) fn plausible_table_address(address: usize) -> bool {
    address >= DRAM_BASE && address < DRAM_END && address % GRANULE == 0
}

pub(super) fn descriptor_address(descriptor_or_address: u64) -> u64 {
    descriptor_or_address & ADDRESS_MASK
}

/// Read-only DMA alias for a surface shared with another task (for example a
/// compositor scanning a client-owned buffer). EL0 may read the pixels but
/// cannot modify them; the owner keeps its writable alias.

pub(super) unsafe fn zero_page(physical: usize) {
    unsafe { core::ptr::write_bytes(phys_to_virt(physical) as *mut u8, 0, GRANULE) };
}

pub(super) unsafe fn write_entry(physical: usize, index: usize, descriptor: u64) {
    unsafe {
        core::ptr::write_volatile((phys_to_virt(physical) as *mut u64).add(index), descriptor)
    };
}

pub(super) unsafe fn read_entry(physical: usize, index: usize) -> u64 {
    unsafe { core::ptr::read_volatile((phys_to_virt(physical) as *const u64).add(index)) }
}

pub(super) unsafe fn follow_table(physical: usize, index: usize) -> usize {
    descriptor_address(read_entry(physical, index)) as usize
}
