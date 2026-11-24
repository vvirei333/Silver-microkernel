use super::*;

struct FutexWaiter {
    address: u64,
    tgid: u64,
    task: u64,
    woken: bool,
}

static FUTEX_WAITERS: Mutex<Vec<FutexWaiter>> = Mutex::new(Vec::new());

// Linux AArch64 syscall numbers (asm-generic/unistd.h).

pub(super) fn linux_brk(requested: u64) -> u64 {
    let current = scheduler::change_program_break(0).unwrap_or(0) as u64;
    if requested == 0 {
        return current;
    }
    scheduler::change_program_break(requested as usize)
        .map(|address| address as u64)
        .unwrap_or(current)
}

/// `futex(2)`: without thread support a contended wait cannot be woken by
/// another task in the same address space. `FUTEX_WAIT` therefore validates
/// the value, sleeps for the requested timeout (or 10 ms when none is given)
/// and reports `-ETIMEDOUT`; `FUTEX_WAKE` reports zero waiters. This keeps
/// single-threaded libc paths working without pretending threads exist.
pub(super) fn linux_futex(address: u64, operation: u64, value: u64, timeout: u64) -> u64 {
    const FUTEX_WAIT: u64 = 0;
    const FUTEX_WAKE: u64 = 1;
    const FUTEX_WAIT_BITSET: u64 = 9;
    const FUTEX_WAKE_BITSET: u64 = 10;
    const ETIMEDOUT: u64 = 110;
    let task = scheduler::current_task_id();
    let tgid = scheduler::current_tgid();
    match operation & 0x7f {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let mut raw = [0_u8; 4];
            if user_memory::copy_from_user(address, &mut raw).is_err() {
                return errno(EFAULT);
            }
            if u32::from_le_bytes(raw) != value as u32 {
                return errno(EAGAIN);
            }
            let deadline = if timeout == 0 {
                None
            } else {
                let mut bytes = [0_u8; 16];
                if user_memory::copy_from_user(timeout, &mut bytes).is_err() {
                    return errno(EFAULT);
                }
                let seconds = i64::from_le_bytes(bytes[..8].try_into().unwrap());
                let nanoseconds = i64::from_le_bytes(bytes[8..].try_into().unwrap());
                if seconds < 0 {
                    None
                } else {
                    let millis = (seconds as u64).saturating_mul(1000)
                        + nanoseconds.max(0) as u64 / 1_000_000;
                    Some(
                        timer::ticks().saturating_add(timer::milliseconds_to_ticks(millis)),
                    )
                }
            };
            FUTEX_WAITERS.lock().push(FutexWaiter {
                address,
                tgid,
                task,
                woken: false,
            });
            loop {
                {
                    let mut waiters = FUTEX_WAITERS.lock();
                    let Some(entry) = waiters
                        .iter()
                        .find(|entry| entry.task == task && entry.address == address)
                    else {
                        return 0;
                    };
                    if entry.woken {
                        waiters.retain(|entry| entry.task != task || entry.address != address);
                        return 0;
                    }
                }
                let mut current = [0_u8; 4];
                if user_memory::copy_from_user(address, &mut current).is_err() {
                    futex_cleanup(task);
                    return errno(EFAULT);
                }
                if u32::from_le_bytes(current) != value as u32 {
                    futex_cleanup(task);
                    return 0;
                }
                if let Some(deadline) = deadline {
                    if timer::ticks() >= deadline {
                        futex_cleanup(task);
                        return errno(ETIMEDOUT);
                    }
                }
                scheduler::sleep(1);
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            futex_wake_address(address, tgid, value as usize) as u64
        }
        _ => errno(ENOSYS),
    }
}

/// Wakes up to `count` waiters on `(address, tgid)`. Returns how many were
/// marked; `wake_for_signal` also interrupts their polling sleep.
pub fn futex_wake_address(address: u64, tgid: u64, count: usize) -> usize {
    let mut wake = Vec::new();
    {
        let mut waiters = FUTEX_WAITERS.lock();
        for entry in waiters.iter_mut() {
            if wake.len() >= count {
                break;
            }
            if entry.address == address && entry.tgid == tgid && !entry.woken {
                entry.woken = true;
                wake.push(entry.task);
            }
        }
    }
    for task in &wake {
        scheduler::wake_for_signal(*task);
    }
    wake.len()
}

pub fn futex_cleanup(task: u64) {
    FUTEX_WAITERS.lock().retain(|entry| entry.task != task);
}

pub(super) fn linux_mmap(
    address: u64,
    length: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    offset: u64,
) -> u64 {
    const MAP_SHARED: u64 = 0x01;
    const MAP_PRIVATE: u64 = 0x02;
    const MAP_FIXED: u64 = 0x10;
    const MAP_ANONYMOUS: u64 = 0x20;
    const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;
    const PROT_WRITE: u64 = 0x2;
    const PAGE_SIZE: u64 = crate::memory::PAGE_SIZE as u64;

    if length == 0 {
        return errno(EINVAL);
    }
    // Read permission is implied by every mapping we create; reject unknown
    // protection bits rather than silently ignoring them.
    if prot & !0x7 != 0 {
        return errno(EINVAL);
    }
    if flags & (MAP_PRIVATE | MAP_SHARED) == 0 {
        return errno(EINVAL);
    }
    let fixed_bits = MAP_FIXED | MAP_FIXED_NOREPLACE;
    let fixed = if flags & fixed_bits != 0 {
        if address == 0 || address % PAGE_SIZE != 0 {
            return errno(EINVAL);
        }
        Some(address as usize)
    } else {
        // A hint without MAP_FIXED is advisory; first-fit ignores it.
        None
    };

    if flags & MAP_ANONYMOUS != 0 {
        // Anonymous memory is lazily backed, like Linux: the virtual range is
        // reserved now and each page is committed when it is first touched.
        // Eager backing charged the whole span up front, so a single 8 MiB
        // `pthread_create` stack cost 2048 frames of the per-task budget even
        // though only a handful of its pages are ever used; a GTK-sized
        // process (most of its budget already spent on library mappings) then
        // failed the charge and thread creation returned EAGAIN. A pure
        // `PROT_NONE` reservation was already handled this way (glibc's
        // 128 MiB `new_heap` probe, ld.so's library spans).
        let lazy = true;
        let placed = match fixed {
            Some(address) if lazy => {
                scheduler::mmap_anonymous_at_lazy(length as usize, prot, address)
            }
            Some(address) => scheduler::mmap_anonymous_at(length as usize, prot, address),
            None if lazy => scheduler::mmap_anonymous_lazy(length as usize, prot),
            None => scheduler::mmap_anonymous(length as usize, prot),
        };
        scheduler::mmap_ring_record(0, placed.unwrap_or(0), length as usize, prot | flags);
        return placed.map(|placed| placed as u64).unwrap_or(errno(ENOMEM));
    }

    // File-backed mapping. Linux requires a page-aligned file offset.
    if offset % PAGE_SIZE != 0 {
        return errno(EINVAL);
    }
    // A `MAP_SHARED` memfd is a true shared window: every process that maps
    // the same file (passed over SCM_RIGHTS) sees the same frames. This is
    // what Wayland's `wl_shm` buffers use. Other files still copy into
    // private frames, and a writable shared mapping of a read-only SFS file
    // is refused rather than silently losing its write-back.
    if flags & MAP_SHARED != 0 && linux_fd::is_prime(scheduler::current_task_id(), fd) {
        let task = scheduler::current_task_id();
        let Some((pa, size)) = linux_fd::prime_info(task, fd) else {
            return errno(EINVAL);
        };
        if length as usize > size {
            return errno(EINVAL);
        }
        return crate::drm::map_prime(pa + offset as usize, length as usize)
            .map(|placed| placed as u64)
            .unwrap_or(errno(EINVAL));
    }
    if flags & MAP_SHARED != 0 && linux_fd::is_drm(scheduler::current_task_id(), fd) {
        // DRM dumb buffers: the offset is the buffer's guest-physical address,
        // handed out by DRM_IOCTL_MODE_MAP_DUMB.
        if offset == 0 {
            return errno(EINVAL);
        }
        return crate::drm::map_dumb(length as usize, offset as usize)
            .map(|placed| placed as u64)
            .unwrap_or(errno(EINVAL));
    }
    if flags & MAP_SHARED != 0 && linux_fd::is_gpufb(scheduler::current_task_id(), fd) {
        // The virtio-gpu scanout. The kernel owns the contiguous frames; the
        // mapping is Non-Cacheable so the device sees the compositor's writes.
        if offset != 0 {
            return errno(EINVAL);
        }
        return scheduler::map_gpufb(length as usize)
            .map(|placed| placed as u64)
            .unwrap_or(errno(ENOMEM));
    }
    if flags & MAP_SHARED != 0
        && linux_fd::is_ivshmem(scheduler::current_task_id(), fd)
    {
        // Host-shared display surface. The kernel owns the BAR placement and
        // maps it Non-Cacheable; only offset 0 of the window is meaningful.
        if offset != 0 || length as usize > crate::ivshmem::BAR_SIZE {
            return errno(EINVAL);
        }
        return scheduler::map_ivshmem(length as usize)
            .map(|placed| placed as u64)
            .unwrap_or(errno(ENOMEM));
    }
    if flags & MAP_SHARED != 0 {
        let task = scheduler::current_task_id();
        if let Ok((physical, owner)) =
            linux_fd::memfd_shared(task, fd, offset as usize, length as usize)
        {
            let placed = scheduler::mmap_shared_file(
                length as usize,
                prot,
                fixed,
                physical,
                owner,
            );
            return placed.map(|placed| placed as u64).unwrap_or(errno(ENOMEM));
        }
        if prot & PROT_WRITE != 0 {
            return errno(EINVAL);
        }
    }
    // MAP_PRIVATE: copy the file range into fresh frames, one page at a time.
    // A library segment is routinely larger than the whole-file read cap
    // (`MAX_FILE_BYTES`, 8 MiB — libgtk-3 is ~8.5 MiB), and the contiguous
    // allocator cannot find a 2000-page run after a few hundred mappings.
    // Mesa's `libgallium` pulls in `libLLVM`, which is 171 MiB; the loader
    // maps each PT_LOAD segment with one mmap. The mapping is still paged
    // (one frame per file page), so the cap only bounds a single request.
    const MAX_FILE_MMAP_BYTES: usize = 256 * 1024 * 1024;
    if length as usize > MAX_FILE_MMAP_BYTES {
        return errno(ENOMEM);
    }
    let task = scheduler::current_task_id();
    // A file from the (immutable) filesystem is mapped lazily: the loader maps
    // whole PT_LOAD segments, and an eager copy of libLLVM alone cost 7.7 s of
    // block-device reads for pages the program never executes. memfd and other
    // synthetic descriptors keep the eager path below.
    if let Some(path) = linux_fd::vfs_path(task, fd) {
        let placed = scheduler::mmap_file_lazy(
            length as usize,
            prot,
            fixed,
            path,
            offset as usize,
        );
        scheduler::mmap_ring_record(1, placed.unwrap_or(0), length as usize, prot | flags);
        return placed.map(|placed| placed as u64).unwrap_or(errno(ENOMEM));
    }
    let placed = scheduler::mmap_file_paged(length as usize, prot, fixed, |page_offset, buffer| {
        // The buffer arrives zeroed; a short read leaves the rest as the
        // zero-fill Linux expects past EOF.
        linux_fd::pread(task, fd, offset + page_offset as u64, buffer).is_ok()
    });
    placed.map(|placed| placed as u64).unwrap_or(errno(ENOMEM))
}

pub(super) fn linux_mprotect(address: u64, length: u64, prot: u64) -> u64 {
    const PAGE_SIZE: u64 = crate::memory::PAGE_SIZE as u64;
    if address % PAGE_SIZE != 0 || prot & !0x7 != 0 {
        return errno(EINVAL);
    }
    if length == 0 {
        return 0;
    }
    if scheduler::mprotect(address as usize, length as usize, prot) {
        0
    } else {
        crate::console::write_str("LINUX: mprotect failed addr=0x");
        crate::console::write_hex_u64(address);
        crate::console::write_str(" len=0x");
        crate::console::write_hex_u64(length);
        crate::console::write_str(" prot=0x");
        crate::console::write_hex_u64(prot);
        crate::console::write_str("\r\n");
        errno(ENOMEM)
    }
}

pub(super) fn linux_munmap(address: u64, length: u64) -> u64 {
    if address == 0 || length == 0 {
        return errno(EINVAL);
    }
    scheduler::mmap_ring_record(3, address as usize, length as usize, 0);
    if scheduler::munmap(address as usize, length as usize) {
        0
    } else {
        errno(EINVAL)
    }
}
