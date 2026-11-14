// Excerpts from src/mmu.rs: remap/COW helpers (1560-1720) and user-page walk (3160-3235)
        return false;
    }
    let root = root_of(ttbr0);
    unsafe {
        let l0_entry = read_entry(root, PRIVATE_WINDOW_L0_INDEX);
        if l0_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l1 = descriptor_address(l0_entry) as usize;
        let l1_entry = read_entry(l1, 0);
        if l1_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l2 = descriptor_address(l1_entry) as usize;
        let l2_entry = read_entry(l2, 0);
        if l2_entry & DESC_TABLE != DESC_TABLE {
            return false;
        }
        let l3 = descriptor_address(l2_entry) as usize;
        if read_entry(l3, 0) & DESC_PAGE != DESC_PAGE {
            return false;
        }
        write_entry(l3, 0, user_data_page(physical));
        clean_entry_line(l3, 0);
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Re-points an existing user range at a new physical run with one uniform
/// protection. Used by the COW fault path after it copied a stack/heap/mmap
/// run. All target slots must already be live pages.
pub fn remap_user_pages_at(
    ttbr0: usize,
    virtual_address: usize,
    physical_start: usize,
    page_count: usize,
    prot: u64,
) -> bool {
    if page_count == 0
        || virtual_address % GRANULE != 0
        || physical_start % GRANULE != 0
        || (prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0)
    {
        return false;
    }
    let Some(span) = page_count.checked_mul(GRANULE) else {
        return false;
    };
    let Some(end) = virtual_address.checked_add(span) else {
        return false;
    };
    if virtual_address < GRANULE || end > USER_STACK_VA + USER_STACK_SIZE {
        return false;
    }
    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2_entry = read_entry(l1_user, l1_index);
            if l2_entry & DESC_TABLE != DESC_TABLE {
                return false;
            }
            let l2 = descriptor_address(l2_entry) as usize;
            let l3_entry = read_entry(l2, l2_index);
            if l3_entry & DESC_TABLE != DESC_TABLE {
                return false;
            }
            let l3 = descriptor_address(l3_entry) as usize;
            if read_entry(l3, l3_index) & DESC_PAGE != DESC_PAGE {
                return false;
            }
        }
        for index in 0..page_count {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2 = descriptor_address(read_entry(l1_user, l1_index)) as usize;
            let l3 = descriptor_address(read_entry(l2, l2_index)) as usize;
            write_entry(
                l3,
                l3_index,
                user_anonymous_page(physical_start + index * GRANULE, prot),
            );
            clean_entry_line(l3, l3_index);
        }
    }
    finish_table_changes_for(ttbr0);
    true
}

/// Re-points an existing image range at a new physical run, preserving each
/// page's permission class. The COW path uses this after copying a whole
/// image run and promoting `Cow` pages back to `Data`.
pub fn remap_user_kinds_at(
    ttbr0: usize,
    virtual_address: usize,
    physical_start: usize,
    kinds: &[crate::elf::PageKind],
) -> bool {
    if kinds.is_empty() || virtual_address % GRANULE != 0 || physical_start % GRANULE != 0 {
        return false;
    }
    let Some(span) = kinds.len().checked_mul(GRANULE) else {
        return false;
    };
    let Some(end) = virtual_address.checked_add(span) else {
        return false;
    };
    if virtual_address < GRANULE || end > USER_STACK_VA + USER_STACK_SIZE {
        return false;
    }
    let root = root_of(ttbr0);
    unsafe {
        let l1_user = follow_table(root, 0);
        for index in 0..kinds.len() {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2_entry = read_entry(l1_user, l1_index);
            if l2_entry & DESC_TABLE != DESC_TABLE {
                return false;
            }
            let l2 = descriptor_address(l2_entry) as usize;
            let l3_entry = read_entry(l2, l2_index);
            if l3_entry & DESC_TABLE != DESC_TABLE {
                return false;
            }
            let l3 = descriptor_address(l3_entry) as usize;
            if read_entry(l3, l3_index) & DESC_PAGE != DESC_PAGE {
                return false;
            }
        }
        for (index, kind) in kinds.iter().enumerate() {
            let va = virtual_address + index * GRANULE;
            let l1_index = (va >> 30) & (ENTRIES - 1);
            let l2_index = (va >> 21) & (ENTRIES - 1);
            let l3_index = (va >> 12) & 0x1FF;
            let l2 = descriptor_address(read_entry(l1_user, l1_index)) as usize;
            let l3 = descriptor_address(read_entry(l2, l2_index)) as usize;
            let physical = physical_start + index * GRANULE;
            let descriptor = match kind {
                crate::elf::PageKind::Text => user_text_page(physical),
                crate::elf::PageKind::ReadOnly | crate::elf::PageKind::Cow => {
                    user_rodata_page(physical)
                }
                crate::elf::PageKind::Data => user_data_page(physical),
            };
            write_entry(l3, l3_index, descriptor);
            clean_entry_line(l3, l3_index);
        }
    }
    finish_table_changes_for(ttbr0);
    true
}

    if length == 0 {
        return true;
    }
    let Some(last) = pointer.checked_add(length - 1) else {
        return false;
    };
    if last >= (1_u64 << 48) {
        return false;
    }

    let mut page = pointer & !(GRANULE as u64 - 1);
    let last_page = last & !(GRANULE as u64 - 1);
    loop {
        if !current_user_page_access(page, write) {
            return false;
        }
        if page == last_page {
            return true;
        }
        page += GRANULE as u64;
    }
}

fn current_user_page_access(virtual_address: u64, write: bool) -> bool {
    let ttbr0: u64;
    unsafe {
        core::arch::asm!("mrs {value}, ttbr0_el1", value = out(reg) ttbr0, options(nostack));
        let mut table = descriptor_address(ttbr0) as usize;
        for shift in [39, 30, 21] {
            let index = ((virtual_address >> shift) & 0x1FF) as usize;
            let descriptor = read_entry(table, index);
            if descriptor & DESC_TABLE != DESC_TABLE {
                return false;
            }
            table = descriptor_address(descriptor) as usize;
        }
        let index = ((virtual_address >> 12) & 0x1FF) as usize;
        let descriptor = read_entry(table, index);
        if descriptor & DESC_PAGE != DESC_PAGE {
            return false;
        }
        let access_permissions = (descriptor >> 6) & 0b11;
        if write {
            access_permissions == 0b01
        } else {
            access_permissions == 0b01 || access_permissions == 0b11
        }
    }
}

fn descriptor_address(descriptor_or_address: u64) -> u64 {
    descriptor_or_address & ADDRESS_MASK
}

/// User ELF text: readable/executable at EL0, never executable at EL1.
fn user_text_page(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
        | AP_BOTH_RO
        | SH_INNER
        | AF
        | NG
        | PXN
}

/// User ELF rodata: read-only and never executable at either level.
fn user_rodata_page(output_address: usize) -> u64 {
    user_text_page(output_address) | UXN
}

/// Task-owned writable memory: user stack or private page, never executable.
fn user_data_page(output_address: usize) -> u64 {
    descriptor_address(output_address as u64)
        | DESC_PAGE
        | (ATTR_NORMAL << 2)
