use super::*;

/// `pipe2(2)`: installs a read and a write descriptor.
pub(super) fn linux_epoll_create(flags: u64) -> u64 {
    // EPOLL_CLOEXEC is the only flag; this ABI has no exec-time descriptor
    // closing anyway.
    if flags & !0x80000 != 0 {
        return errno(EINVAL);
    }
    finish(linux_fd::epoll_create(scheduler::current_task_id()))
}

pub(super) fn linux_epoll_ctl(epfd: u64, operation: u64, fd: u64, event_pointer: u64) -> u64 {
    const EPOLL_CTL_DEL: u64 = 2;
    let (events, data) = if operation == EPOLL_CTL_DEL {
        (0u32, 0u64)
    } else {
        if event_pointer == 0 {
            return errno(EFAULT);
        }
        let mut raw = [0_u8; 16];
        if user_memory::copy_from_user(event_pointer, &mut raw).is_err() {
            return errno(EFAULT);
        }
        (
            u32::from_le_bytes(raw[0..4].try_into().unwrap()),
            u64::from_le_bytes(raw[8..16].try_into().unwrap()),
        )
    };
    finish(
        linux_fd::epoll_ctl(
            scheduler::current_task_id(),
            epfd,
            operation,
            fd,
            events,
            data,
        )
        .map(|_| 0),
    )
}

pub(super) fn linux_epoll_wait(
    epfd: u64,
    events_pointer: u64,
    max_events: u64,
    timeout_ms: i64,
) -> u64 {
    // glibc's `epoll_pwait` asks for 1024 slots; only the entries actually
    // ready are written, so any user-chosen size is safe here. Reject only a
    // zero or absurd request.
    if max_events == 0 || max_events > 65_536 {
        return errno(EINVAL);
    }
    let ready = match linux_fd::epoll_wait(
        scheduler::current_task_id(),
        epfd,
        max_events as usize,
        timeout_ms,
    ) {
        Ok(ready) => ready,
        Err(code) => return errno(code),
    };
    for (index, (events, data)) in ready.iter().enumerate() {
        let mut raw = [0_u8; 16];
        raw[0..4].copy_from_slice(&events.to_le_bytes());
        raw[8..16].copy_from_slice(&data.to_le_bytes());
        if user_memory::copy_to_user(&raw, events_pointer + (index * 16) as u64).is_err() {
            return errno(EFAULT);
        }
    }
    ready.len() as u64
}

/// `epoll_pwait2`: the timeout is a `timespec` pointer, not milliseconds.
pub(super) fn linux_epoll_wait2(
    epfd: u64,
    events_pointer: u64,
    max_events: u64,
    timeout_pointer: u64,
) -> u64 {
    let timeout_ms = if timeout_pointer == 0 {
        -1
    } else {
        let mut raw = [0_u8; 16];
        if user_memory::copy_from_user(timeout_pointer, &mut raw).is_err() {
            return errno(EFAULT);
        }
        let seconds = i64::from_le_bytes(raw[..8].try_into().unwrap());
        let nanoseconds = i64::from_le_bytes(raw[8..].try_into().unwrap());
        if seconds < 0 {
            -1
        } else {
            seconds
                .saturating_mul(1000)
                .saturating_add(nanoseconds / 1_000_000)
        }
    };
    linux_epoll_wait(epfd, events_pointer, max_events, timeout_ms)
}

pub(super) fn ticks_from_timespec(seconds: u64, nanoseconds: u64) -> u64 {
    seconds
        .saturating_mul(100)
        .saturating_add(nanoseconds / 10_000_000)
}

pub(super) fn write_itimerspec(pointer: u64, interval: u64, remaining: u64) -> Result<(), u64> {
    let mut raw = [0_u8; 32];
    raw[0..8].copy_from_slice(&(interval / 100).to_le_bytes());
    raw[8..16].copy_from_slice(&((interval % 100) * 10_000_000).to_le_bytes());
    raw[16..24].copy_from_slice(&(remaining / 100).to_le_bytes());
    raw[24..32].copy_from_slice(&((remaining % 100) * 10_000_000).to_le_bytes());
    user_memory::copy_to_user(&raw, pointer).map_err(|_| EFAULT)
}

pub(super) fn linux_timerfd_create(_clock: u64, flags: u64) -> u64 {
    if flags & !(0x800 | 0x80000) != 0 {
        return errno(EINVAL);
    }
    finish(linux_fd::timerfd_create(
        scheduler::current_task_id(),
        flags & 0x800 != 0,
    ))
}

pub(super) fn linux_timerfd_settime(fd: u64, flags: u64, new_pointer: u64, old_pointer: u64) -> u64 {
    const TFD_TIMER_ABSTIME: u64 = 1;
    if flags & !3 != 0 || new_pointer == 0 {
        return errno(EINVAL);
    }
    let task = scheduler::current_task_id();
    if old_pointer != 0 {
        let (interval, remaining) = match linux_fd::timerfd_gettime(task, fd) {
            Ok(state) => state,
            Err(code) => return errno(code),
        };
        if write_itimerspec(old_pointer, interval, remaining).is_err() {
            return errno(EFAULT);
        }
    }
    let mut raw = [0_u8; 32];
    if user_memory::copy_from_user(new_pointer, &mut raw).is_err() {
        return errno(EFAULT);
    }
    let interval = ticks_from_timespec(
        u64::from_le_bytes(raw[0..8].try_into().unwrap()),
        u64::from_le_bytes(raw[8..16].try_into().unwrap()),
    );
    let value = ticks_from_timespec(
        u64::from_le_bytes(raw[16..24].try_into().unwrap()),
        u64::from_le_bytes(raw[24..32].try_into().unwrap()),
    );
    finish(
        linux_fd::timerfd_settime(task, fd, interval, value, flags & TFD_TIMER_ABSTIME != 0)
            .map(|_| 0),
    )
}

pub(super) fn linux_timerfd_gettime(fd: u64, current_pointer: u64) -> u64 {
    let (interval, remaining) =
        match linux_fd::timerfd_gettime(scheduler::current_task_id(), fd) {
            Ok(state) => state,
            Err(code) => return errno(code),
        };
    if write_itimerspec(current_pointer, interval, remaining).is_err() {
        return errno(EFAULT);
    }
    0
}

pub(super) fn linux_pipe2(fds_pointer: u64, flags: u64) -> u64 {
    const O_NONBLOCK: u64 = 0o4000;
    const O_CLOEXEC: u64 = 0o2000000;
    let task = scheduler::current_task_id();
    let Ok((read_fd, write_fd)) = linux_fd::pipe(task) else {
        return errno(EMFILE);
    };
    if flags & O_NONBLOCK != 0 {
        let _ = linux_fd::set_nonblock(task, read_fd, true);
        let _ = linux_fd::set_nonblock(task, write_fd, true);
    }
    if flags & O_CLOEXEC != 0 {
        let _ = linux_fd::set_cloexec(task, read_fd, true);
        let _ = linux_fd::set_cloexec(task, write_fd, true);
    }
    let mut pair = [0_u8; 8];
    pair[..4].copy_from_slice(&(read_fd as u32).to_le_bytes());
    pair[4..].copy_from_slice(&(write_fd as u32).to_le_bytes());
    if user_memory::copy_to_user(&pair, fds_pointer).is_err() {
        let _ = linux_fd::close(task, read_fd);
        let _ = linux_fd::close(task, write_fd);
        return errno(EFAULT);
    }
    0
}

/// `poll(2)`: same readiness loop as `ppoll`, timeout in milliseconds
/// (-1 = block, capped like `ppoll` so a stuck poll cannot wedge the kernel).
/// seatd's main loop lives on this call; without it glibc `poll()` returns
/// ENOSYS and the daemon exits immediately.
pub(super) fn linux_poll(
    fds_pointer: u64,
    nfds: u64,
    timeout_ms: i64,
) -> Result<u64, u64> {
    let timeout = if timeout_ms < 0 {
        None
    } else {
        Some(timeout_ms as u64)
    };
    poll_loop(fds_pointer, nfds, timeout)
}

/// `ppoll(2)`: readiness for up to 32 descriptors, with a timeout. A null
/// timeout blocks, but is capped at 60 s so a stuck poll cannot wedge the
/// kernel forever (there is no signal delivery to break it yet).
pub(super) fn linux_ppoll(
    fds_pointer: u64,
    nfds: u64,
    timeout_pointer: u64,
) -> Result<u64, u64> {
    let timeout_ms = if timeout_pointer == 0 {
        None
    } else {
        let mut bytes = [0_u8; 16];
        user_memory::copy_from_user(timeout_pointer, &mut bytes).map_err(|_| EFAULT)?;
        let seconds = i64::from_le_bytes(bytes[..8].try_into().unwrap());
        let nanoseconds = i64::from_le_bytes(bytes[8..].try_into().unwrap());
        if seconds < 0 || nanoseconds < 0 {
            None
        } else {
            Some(
                (seconds as u64)
                    .saturating_mul(1000)
                    .saturating_add(nanoseconds as u64 / 1_000_000),
            )
        }
    };
    poll_loop(fds_pointer, nfds, timeout_ms)
}

pub(super) fn poll_loop(
    fds_pointer: u64,
    nfds: u64,
    timeout_ms: Option<u64>,
) -> Result<u64, u64> {
    const POLLERR: i16 = 0x008;
    const POLLHUP: i16 = 0x010;
    const POLLNVAL: i16 = 0x020;
    if nfds > 32 {
        return Err(EINVAL);
    }
    let count = nfds as usize;
    let start = timer::ticks();
    let deadline = timeout_ms.map(|ms| start + timer::milliseconds_to_ticks(ms));
    loop {
        let mut ready = 0u64;
        let mut entries: Vec<(i32, i16, i16)> = Vec::with_capacity(count);
        for index in 0..count {
            let address = fds_pointer + (index * 8) as u64;
            let mut raw = [0_u8; 8];
            user_memory::copy_from_user(address, &mut raw).map_err(|_| EFAULT)?;
            let fd = i32::from_le_bytes(raw[0..4].try_into().unwrap());
            let events = i16::from_le_bytes(raw[4..6].try_into().unwrap());
            let revents = if fd < 0 {
                0
            } else {
                match linux_fd::poll_ready(scheduler::current_task_id(), fd as u64) {
                    Some(bits) => bits & (events | POLLERR | POLLHUP),
                    None => POLLNVAL,
                }
            };
            if revents != 0 {
                ready += 1;
            }
            entries.push((fd, events, revents));
        }
        let expired = match deadline {
            Some(deadline) => timer::ticks() >= deadline,
            None => timer::ticks().saturating_sub(start) >= 6000,
        };
        if ready != 0 || expired || timeout_ms == Some(0) {
            for (index, &(fd, events, revents)) in entries.iter().enumerate() {
                let mut raw = [0_u8; 8];
                raw[0..4].copy_from_slice(&fd.to_le_bytes());
                raw[4..6].copy_from_slice(&events.to_le_bytes());
                raw[6..8].copy_from_slice(&revents.to_le_bytes());
                user_memory::copy_to_user(&raw, fds_pointer + (index * 8) as u64)
                    .map_err(|_| EFAULT)?;
            }
            return Ok(ready);
        }
        scheduler::sleep(1);
    }
}

/// One `struct msghdr` from userspace (LP64 layout).
pub(super) struct MsgHdr {
    iov: u64,
    iovlen: u64,
    control: u64,
    controllen: u64,
    flags: i32,
}

pub(super) fn read_msghdr(pointer: u64) -> Result<MsgHdr, u64> {
    if pointer == 0 {
        return Err(EFAULT);
    }
    let mut raw = [0_u8; 56];
    user_memory::copy_from_user(pointer, &mut raw).map_err(|_| EFAULT)?;
    Ok(MsgHdr {
        iov: u64::from_le_bytes(raw[16..24].try_into().unwrap()),
        iovlen: u64::from_le_bytes(raw[24..32].try_into().unwrap()),
        control: u64::from_le_bytes(raw[32..40].try_into().unwrap()),
        controllen: u64::from_le_bytes(raw[40..48].try_into().unwrap()),
        flags: i32::from_le_bytes(raw[48..52].try_into().unwrap()),
    })
}

/// Gathers the `iovec` array into one contiguous payload.
pub(super) fn read_iovecs(header: &MsgHdr, limit: usize) -> Result<Vec<u8>, u64> {
    if header.iovlen > 64 {
        return Err(EINVAL);
    }
    let mut bytes = Vec::new();
    for index in 0..header.iovlen {
        let mut raw = [0_u8; 16];
        user_memory::copy_from_user(header.iov + index * 16, &mut raw).map_err(|_| EFAULT)?;
        let base = u64::from_le_bytes(raw[..8].try_into().unwrap());
        let length = u64::from_le_bytes(raw[8..].try_into().unwrap());
        let length = usize::try_from(length).map_err(|_| EINVAL)?;
        if bytes.len().saturating_add(length) > limit {
            return Err(EINVAL);
        }
        let start = bytes.len();
        bytes.resize(start + length, 0);
        if length != 0 {
            user_memory::copy_from_user(base, &mut bytes[start..]).map_err(|_| EFAULT)?;
        }
    }
    Ok(bytes)
}

/// Scatters up to `bytes.len()` bytes back into the caller's `iovec` array.
pub(super) fn write_iovecs(header: &MsgHdr, bytes: &[u8]) -> Result<(), u64> {
    let mut offset = 0usize;
    for index in 0..header.iovlen {
        if offset == bytes.len() {
            break;
        }
        let mut raw = [0_u8; 16];
        user_memory::copy_from_user(header.iov + index * 16, &mut raw).map_err(|_| EFAULT)?;
        let base = u64::from_le_bytes(raw[..8].try_into().unwrap());
        let length = u64::from_le_bytes(raw[8..].try_into().unwrap());
        let length = (length as usize).min(bytes.len() - offset);
        if length != 0 {
            user_memory::copy_to_user(&bytes[offset..offset + length], base)
                .map_err(|_| EFAULT)?;
            offset += length;
        }
    }
    Ok(())
}

/// Extracts `SCM_RIGHTS` descriptors from a control buffer.
pub(super) fn parse_rights(control: u64, controllen: u64) -> Result<Vec<u64>, u64> {
    let mut fds = Vec::new();
    if control == 0 || controllen < 16 || controllen > 4096 {
        return Ok(fds);
    }
    let mut raw = alloc::vec![0_u8; controllen as usize];
    user_memory::copy_from_user(control, &mut raw).map_err(|_| EFAULT)?;
    let mut offset = 0usize;
    while offset + 16 <= raw.len() {
        let length = u64::from_le_bytes(raw[offset..offset + 8].try_into().unwrap());
        let level = i32::from_le_bytes(raw[offset + 8..offset + 12].try_into().unwrap());
        let kind = i32::from_le_bytes(raw[offset + 12..offset + 16].try_into().unwrap());
        if length < 16 {
            break;
        }
        // `length` is attacker-controlled: a plain `offset + length` wraps in
        // release builds. A wrapped sum used to pass the bound check and then
        // made `offset` wrap backwards, so the loop never advanced and the
        // kernel spun forever holding the kernel lock. Every step is checked
        // and must make strict forward progress.
        let end = match (offset as u64).checked_add(length) {
            Some(end) if end <= controllen => end as usize,
            _ => break,
        };
        if level == SOL_SOCKET as i32 && kind == SCM_RIGHTS as i32 {
            let count = (end - offset - 16) / 4;
            if count > 64 {
                return Err(EINVAL);
            }
            for index in 0..count {
                let start = offset + 16 + index * 4;
                let fd = i32::from_le_bytes(raw[start..start + 4].try_into().unwrap());
                if fd >= 0 {
                    fds.push(fd as u64);
                }
            }
        }
        let advance = match (length as usize).checked_add(7) {
            Some(value) => value & !7,
            None => break,
        };
        offset = match offset.checked_add(advance) {
            Some(next) if next > offset => next,
            _ => break,
        };
    }
    Ok(fds)
}

/// Writes one `SCM_RIGHTS` control message. Returns flags to merge into
/// `msg_flags` (`MSG_CTRUNC` when the buffer was too small; those descriptors
/// are closed, as Linux does).
pub(super) fn write_rights(task: u64, control: u64, controllen: u64, fds: &[u64]) -> i32 {
    if fds.is_empty() {
        return 0;
    }
    let needed = 16 + fds.len() * 4;
    let spaced = (needed + 7) & !7;
    if control == 0 || (controllen as usize) < spaced {
        for fd in fds {
            let _ = linux_fd::close(task, *fd);
        }
        return MSG_CTRUNC;
    }
    let mut block = alloc::vec![0_u8; spaced];
    block[..8].copy_from_slice(&(needed as u64).to_le_bytes());
    block[8..12].copy_from_slice(&(SOL_SOCKET as i32).to_le_bytes());
    block[12..16].copy_from_slice(&(SCM_RIGHTS as i32).to_le_bytes());
    for (index, fd) in fds.iter().enumerate() {
        let start = 16 + index * 4;
        block[start..start + 4].copy_from_slice(&(*fd as i32).to_le_bytes());
    }
    if user_memory::copy_to_user(&block, control).is_err() {
        return MSG_CTRUNC;
    }
    0
}

pub(super) fn linux_socketpair(domain: u64, kind: u64, protocol: u64, pair: u64) -> u64 {
    if domain != AF_UNIX {
        return errno(EAFNOSUPPORT);
    }
    let base = kind & 0xf;
    // Rust's `Command` creates its exec-error channel with
    // `socketpair(AF_UNIX, SOCK_SEQPACKET|SOCK_CLOEXEC)`. The kernel keeps
    // byte-stream semantics internally, which is sufficient for that channel
    // and for every other `socketpair` user we have.
    if (base != 1 && base != 2 && base != 5) || protocol != 0 {
        return errno(EPROTONOSUPPORT);
    }
    let nonblock = kind & SOCK_NONBLOCK != 0;
    let task = scheduler::current_task_id();
    match linux_fd::socketpair(task, nonblock) {
        Ok((first, second)) => {
            if kind & SOCK_CLOEXEC != 0 {
                let _ = linux_fd::set_cloexec(task, first, true);
                let _ = linux_fd::set_cloexec(task, second, true);
            }
            let mut raw = [0_u8; 8];
            raw[..4].copy_from_slice(&(first as i32).to_le_bytes());
            raw[4..].copy_from_slice(&(second as i32).to_le_bytes());
            if user_memory::copy_to_user(&raw, pair).is_err() {
                let _ = linux_fd::close(task, first);
                let _ = linux_fd::close(task, second);
                return errno(EFAULT);
            }
            0
        }
        Err(code) => errno(code),
    }
}

pub(super) fn linux_sendmsg(fd: u64, message: u64, flags: u64) -> u64 {
    let header = match read_msghdr(message) {
        Ok(header) => header,
        Err(code) => return errno(code),
    };
    let bytes = match read_iovecs(&header, 1 << 20) {
        Ok(bytes) => bytes,
        Err(code) => return errno(code),
    };
    let pass = match parse_rights(header.control, header.controllen) {
        Ok(pass) => pass,
        Err(code) => return errno(code),
    };
    let task = scheduler::current_task_id();
    finish(linux_fd::socket_send(
        task,
        fd,
        &bytes,
        &pass,
        flags & MSG_DONTWAIT != 0,
    ))
}

pub(super) fn linux_recvmsg(fd: u64, message: u64, flags: u64) -> u64 {
    let header = match read_msghdr(message) {
        Ok(header) => header,
        Err(code) => return errno(code),
    };
    let mut total = 0usize;
    for index in 0..header.iovlen.min(64) {
        let mut raw = [0_u8; 16];
        if user_memory::copy_from_user(header.iov + index * 16, &mut raw).is_err() {
            return errno(EFAULT);
        }
        let length = u64::from_le_bytes(raw[8..].try_into().unwrap());
        total = match total.checked_add(length as usize) {
            Some(total) if total <= 1 << 20 => total,
            _ => return errno(EINVAL),
        };
    }
    let mut bytes = alloc::vec![0_u8; total];
    let task = scheduler::current_task_id();
    let (read, fds) = match linux_fd::socket_recv(
        task,
        fd,
        &mut bytes,
        flags & MSG_DONTWAIT != 0,
    ) {
        Ok(result) => result,
        Err(code) => return errno(code),
    };
    if write_iovecs(&header, &bytes[..read]).is_err() {
        return errno(EFAULT);
    }
    let control_flags = write_rights(task, header.control, header.controllen, &fds);
    let mut output = [0_u8; 16];
    let size = if control_flags & MSG_CTRUNC != 0 || fds.is_empty() {
        0u64
    } else {
        ((16 + fds.len() * 4 + 7) & !7) as u64
    };
    output[..8].copy_from_slice(&size.to_le_bytes());
    let msg_flags = (header.flags & !MSG_CTRUNC) | control_flags;
    output[8..12].copy_from_slice(&msg_flags.to_le_bytes());
    if user_memory::copy_to_user(&output, message + 40).is_err() {
        return errno(EFAULT);
    }
    read as u64
}

pub(super) fn linux_sendto(fd: u64, buffer: u64, length: u64, flags: u64) -> u64 {
    let length = (length as usize).min(1 << 20);
    let mut bytes = alloc::vec![0_u8; length];
    if user_memory::copy_from_user(buffer, &mut bytes).is_err() {
        return errno(EFAULT);
    }
    finish(linux_fd::socket_send(
        scheduler::current_task_id(),
        fd,
        &bytes,
        &[],
        flags & MSG_DONTWAIT != 0,
    ))
}

pub(super) fn linux_recvfrom(
    fd: u64,
    buffer: u64,
    length: u64,
    flags: u64,
    address: u64,
    address_length: u64,
) -> u64 {
    let length = (length as usize).min(1 << 20);
    let mut bytes = alloc::vec![0_u8; length];
    let task = scheduler::current_task_id();
    let read = match linux_fd::socket_recv(task, fd, &mut bytes, flags & MSG_DONTWAIT != 0) {
        Ok((read, _)) => read,
        Err(code) => return errno(code),
    };
    if user_memory::copy_to_user(&bytes[..read], buffer).is_err() {
        return errno(EFAULT);
    }
    let _ = write_sockaddr(address, address_length, &[]);
    read as u64
}

/// Reads an `AF_UNIX` `sockaddr_un`. Filesystem names are NUL-terminated;
/// abstract names keep the leading NUL and their full byte length.
pub(super) fn read_sockaddr(address: u64, length: u64) -> Result<Vec<u8>, u64> {
    if address == 0 || length < 3 {
        return Err(EINVAL);
    }
    let length = (length as usize).min(110);
    let mut raw = alloc::vec![0_u8; length];
    user_memory::copy_from_user(address, &mut raw).map_err(|_| EFAULT)?;
    let family = u16::from_le_bytes(raw[0..2].try_into().unwrap());
    if family != AF_UNIX as u16 {
        return Err(EAFNOSUPPORT);
    }
    let body = &raw[2..];
    let path = if body[0] == 0 {
        if body.len() < 2 {
            return Err(EINVAL);
        }
        body.to_vec()
    } else {
        let end = body.iter().position(|byte| *byte == 0).unwrap_or(body.len());
        if end == 0 {
            return Err(EINVAL);
        }
        body[..end].to_vec()
    };
    Ok(path)
}

/// Writes `family + path` into a caller `sockaddr_un` and stores the used
/// length. `path` is empty for an unnamed socket.
pub(super) fn write_sockaddr(address: u64, length_pointer: u64, path: &[u8]) -> Result<(), u64> {
    let mut capacity = 0u32;
    if length_pointer != 0 {
        let mut raw = [0_u8; 4];
        user_memory::copy_from_user(length_pointer, &mut raw).map_err(|_| EFAULT)?;
        capacity = u32::from_le_bytes(raw);
    }
    // Filesystem names are returned NUL-terminated and the length includes
    // the terminator, matching Linux. Abstract names carry their own leading
    // NUL and no extra terminator.
    let filesystem = path.first().map(|byte| *byte != 0).unwrap_or(false);
    let terminator = usize::from(filesystem);
    let total = 2 + path.len() + terminator;
    if address != 0 && capacity >= 2 {
        let family = (AF_UNIX as u16).to_le_bytes();
        user_memory::copy_to_user(&family, address).map_err(|_| EFAULT)?;
        if capacity as usize >= total && !path.is_empty() {
            user_memory::copy_to_user(path, address + 2).map_err(|_| EFAULT)?;
            if terminator != 0 {
                let nul = [0_u8; 1];
                user_memory::copy_to_user(&nul, address + 2 + path.len() as u64)
                    .map_err(|_| EFAULT)?;
            }
        }
    }
    if length_pointer != 0 {
        let used = if address == 0 {
            0
        } else {
            total.min(capacity as usize) as u32
        };
        user_memory::copy_to_user(&used.to_le_bytes(), length_pointer).map_err(|_| EFAULT)?;
    }
    Ok(())
}

pub(super) fn linux_sockname(task: u64, fd: u64, address: u64, length_pointer: u64) -> u64 {
    let path = linux_fd::socket_local_name(task, fd).unwrap_or_default();
    match write_sockaddr(address, length_pointer, &path) {
        Ok(()) => 0,
        Err(code) => errno(code),
    }
}

pub(super) fn linux_socket(domain: u64, kind: u64, protocol: u64) -> u64 {
    if domain == 16 {
        // AF_NETLINK: only libudev's uevent monitor uses it. The stub accepts
        // binds and sends but never reports data (no uevent publisher).
        let base = kind & 0xf;
        if base == 2 || base == 3 || protocol == 15 {
            return finish(linux_fd::netlink_socket(
                scheduler::current_task_id(),
                kind & SOCK_NONBLOCK != 0,
            ));
        }
        return errno(EPROTONOSUPPORT);
    }
    if domain != AF_UNIX {
        return errno(EAFNOSUPPORT);
    }
    let base = kind & 0xf;
    // SOCK_SEQPACKET callers (stream semantics here) are accepted: the
    // deniald startup path creates one before its first session log.
    if (base != 1 && base != 2 && base != 5) || protocol != 0 {
        return errno(EPROTONOSUPPORT);
    }
    finish(linux_fd::socket(
        scheduler::current_task_id(),
        kind & SOCK_NONBLOCK != 0,
    ))
}

pub(super) fn linux_bind(fd: u64, address: u64, length: u64) -> u64 {
    // Netlink binds carry sockaddr_nl, not a unix path; the stub accepts
    // them as-is (libudev binds NETLINK_KOBJECT_UEVENT group 2).
    if linux_fd::is_netlink(scheduler::current_task_id(), fd) {
        return 0;
    }
    let path = match read_sockaddr(address, length) {
        Ok(path) => path,
        Err(code) => return errno(code),
    };
    finish(linux_fd::bind(scheduler::current_task_id(), fd, path).map(|_| 0))
}

pub(super) fn linux_connect(fd: u64, address: u64, length: u64) -> u64 {
    let path = match read_sockaddr(address, length) {
        Ok(path) => path,
        Err(code) => return errno(code),
    };
    finish(linux_fd::connect(scheduler::current_task_id(), fd, path).map(|_| 0))
}

pub(super) fn linux_accept(fd: u64, address: u64, length_pointer: u64, flags: u64) -> u64 {
    let task = scheduler::current_task_id();
    let new_fd = match linux_fd::accept(task, fd, flags & SOCK_NONBLOCK != 0) {
        Ok(new_fd) => new_fd,
        Err(code) => return errno(code),
    };
    // The accepted peer is unnamed in this model (clients do not bind).
    if let Err(code) = write_sockaddr(address, length_pointer, &[]) {
        let _ = linux_fd::close(task, new_fd);
        return errno(code);
    }
    new_fd
}

pub(super) fn linux_setsockopt(_fd: u64, level: u64) -> u64 {
    if level == SOL_SOCKET {
        0
    } else {
        errno(ENOPROTOOPT)
    }
}

pub(super) fn linux_getsockopt(fd: u64, level: u64, name: u64, value: u64, length: u64) -> u64 {
    const SO_PEERCRED: u64 = 17;
    if level != SOL_SOCKET {
        return errno(ENOPROTOOPT);
    }
    let task = scheduler::current_task_id();
    if linux_fd::status_flags(task, fd).is_none() {
        return errno(EBADF);
    }
    if name == SO_PEERCRED {
        // `struct ucred { pid_t pid; uid_t uid; gid_t gid; }`. The Linux
        // personality runs as root, so EXTERNAL authentication sees uid 0 on
        // both sides.
        if !linux_fd::is_socket(task, fd) {
            return errno(ENOTSOCK);
        }
        let mut capacity = 0u32;
        if length != 0 {
            let mut raw = [0_u8; 4];
            if user_memory::copy_from_user(length, &mut raw).is_err() {
                return errno(EFAULT);
            }
            capacity = u32::from_le_bytes(raw);
        }
        if capacity < 12 {
            return errno(EINVAL);
        }
        let mut credentials = [0_u8; 12];
        // zbus asks for peer credentials while negotiating a private
        // connection; a zero pid is "unknown" and some peers reject it.
        let pid = scheduler::current_task_id().min(u32::MAX as u64) as u32;
        credentials[0..4].copy_from_slice(&pid.to_le_bytes());
        credentials[4..8].copy_from_slice(&0u32.to_le_bytes()); // uid
        credentials[8..12].copy_from_slice(&0u32.to_le_bytes()); // gid
        if value != 0 && user_memory::copy_to_user(&credentials, value).is_err() {
            return errno(EFAULT);
        }
        if length != 0 {
            let used = 12u32.to_le_bytes();
            if user_memory::copy_to_user(&used, length).is_err() {
                return errno(EFAULT);
            }
        }
        return 0;
    }
    if linux_fd::status_flags(task, fd).is_none() {
        return errno(EBADF);
    }
    let answer: i32 = match name {
        3 => 1,          // SO_TYPE = SOCK_STREAM
        4 => 0,          // SO_ERROR
        7 | 8 => 212992, // SO_SNDBUF / SO_RCVBUF
        38 => 0,         // SO_PROTOCOL
        39 => AF_UNIX as i32,
        _ => return errno(ENOPROTOOPT),
    };
    if value != 0 {
        if user_memory::copy_to_user(&answer.to_le_bytes(), value).is_err() {
            return errno(EFAULT);
        }
    }
    if length != 0 {
        let size = 4u32.to_le_bytes();
        if user_memory::copy_to_user(&size, length).is_err() {
            return errno(EFAULT);
        }
    }
    0
}
