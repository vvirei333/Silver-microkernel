use super::*;

/// One private Linux syscall multiplexer for the Capsicum-lite operations.
/// The number is far above the Linux ABI range so it cannot collide with a
/// real arm64 syscall. Operations: 0 enter, 1 limit rights, 2 get rights.
pub(super) fn silver_cap(operation: u64, fd: u64, mask: u64) -> u64 {
    let task = scheduler::current_task_id();
    match operation {
        0 => {
            if scheduler::enter_current_task_capability_mode() {
                0
            } else {
                errno(EPERM)
            }
        }
        1 => match linux_fd::limit_rights(task, fd, mask as u32) {
            Ok(_) => 0,
            Err(code) => errno(code),
        },
        2 => match linux_fd::rights(task, fd) {
            Ok(rights) => rights as u64,
            Err(code) => errno(code),
        },
        _ => errno(EINVAL),
    }
}

/// Path operations that name a directory through `AT_FDCWD` are refused in
/// capability mode. A `*at` call with a real descriptor is still allowed;
/// `directory_path` requires `FD_LOOKUP` on that descriptor.
pub(super) fn cap_denies_cwd() -> bool {
    scheduler::current_task_capability_mode()
}