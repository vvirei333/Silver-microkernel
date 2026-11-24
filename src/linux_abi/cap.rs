use super::*;

/// One private Linux syscall multiplexer for the Capsicum-lite operations.
/// The number is far above the Linux ABI range so it cannot collide with a
/// real arm64 syscall. Operations: 0 enter, 1 limit rights, 2 get rights,
/// 3 limit ioctls (fd, user array, count).
pub(super) fn silver_cap(operation: u64, fd: u64, argument: u64, count: u64) -> u64 {
    let task = scheduler::current_task_id();
    match operation {
        0 => {
            if scheduler::enter_current_task_capability_mode() {
                0
            } else {
                errno(EPERM)
            }
        }
        1 => match linux_fd::limit_rights(task, fd, argument as u32) {
            Ok(_) => 0,
            Err(code) => errno(code),
        },
        2 => match linux_fd::rights(task, fd) {
            Ok(rights) => rights as u64,
            Err(code) => errno(code),
        },
        3 => {
            if count > 256 {
                return errno(EINVAL);
            }
            let mut raw = alloc::vec![0_u8; count as usize * 4];
            if count != 0 {
                if argument == 0 {
                    return errno(EFAULT);
                }
                if crate::user_memory::copy_from_user(argument, &mut raw).is_err() {
                    return errno(EFAULT);
                }
            }
            let commands: Vec<u32> = raw
                .chunks_exact(4)
                .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect();
            match linux_fd::limit_ioctls(task, fd, &commands) {
                Ok(_) => 0,
                Err(code) => errno(code),
            }
        }
        _ => errno(EINVAL),
    }
}

/// Path operations that name a directory through `AT_FDCWD` are refused in
/// capability mode. A `*at` call with a real descriptor is still allowed;
/// `directory_path` requires `FD_LOOKUP` on that descriptor.
pub(super) fn cap_denies_cwd() -> bool {
    scheduler::current_task_capability_mode()
}