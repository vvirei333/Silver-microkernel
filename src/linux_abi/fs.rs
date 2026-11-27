use super::*;

/// Kernel namespaces that stay global in the Linux view. Everything else an
/// absolute path names lives in the SFS mount: Linux's `/` **is** the SFS
/// root, the layout a postmarketOS-style rootfs expects.
const GLOBAL_PREFIXES: [&str; 5] = ["/sfs", "/tmp", "/dev", "/proc", "/sys"];

/// Maps a Linux absolute path into the kernel's VFS tree.
///
/// `--no-linux` tasks never call this. `/sfs` stays an alias so the existing
/// Arch-staged Phosh image (which was patched to `/sfs/...`) keeps working.
pub fn map_absolute(path: &str) -> String {
    for prefix in GLOBAL_PREFIXES {
        if path == prefix
            || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/'))
        {
            return String::from(path);
        }
    }
    let mut mapped = String::with_capacity(path.len() + 4);
    mapped.push_str("/sfs");
    mapped.push_str(path);
    mapped
}

/// Resolves a Linux path (relative to `cwd`) into the VFS tree.
pub fn resolve(cwd: &str, path: &str) -> Result<String, ()> {
    if path.starts_with('/') {
        crate::vfs::resolve("/", &map_absolute(path))
    } else {
        crate::vfs::resolve(cwd, path)
    }
}

/// Like [`resolve`], but the result must be a directory.
pub fn resolve_directory(cwd: &str, path: &str) -> Result<String, ()> {
    let resolved = resolve(cwd, path)?;
    if crate::vfs::resolve_directory("/", &resolved).is_ok() {
        Ok(resolved)
    } else {
        Err(())
    }
}

/// The reverse mapping for `getcwd`: hide the `/sfs` mount point.
pub(crate) fn display_path(path: &str) -> String {
    if path == "/sfs" {
        return String::from("/");
    }
    match path.strip_prefix("/sfs/") {
        Some(rest) => {
            let mut shown = String::with_capacity(rest.len() + 1);
            shown.push('/');
            shown.push_str(rest);
            shown
        }
        None => String::from(path),
    }
}

/// Watch ids for the inotify stub.
pub(super) static INOTIFY_WD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

/// `fchmod`/`fchown`: the VFS has no per-inode permission bits, so the call
/// only has to prove the descriptor is alive.
pub(super) fn linux_fchmod(fd: u64) -> u64 {
    if linux_fd::status_flags(scheduler::current_task_id(), fd).is_none() {
        return errno(EBADF);
    }
    0
}

/// `fchmodat`/`fchownat`: same, by path. The path has to resolve.
pub(super) fn linux_fchmodat(dirfd: u64, path_pointer: u64) -> u64 {
    let Some(path) = user_string(path_pointer) else {
        return errno(EFAULT);
    };
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    match resolve(&cwd, &path) {
        Ok(_) => 0,
        Err(_) => errno(ENOENT),
    }
}

/// `close_range(2)`: glib's spawn path uses it to drop inherited descriptors.
/// It calls `close_range(3, ~0U, 0)`; walking to `u32::MAX` one descriptor at
/// a time burned a core forever before the child could exec (the terminal
/// "froze" on its first command). Only descriptor numbers the fd table can
/// hold exist, so clamp to its limit.
pub(super) fn linux_close_range(first: u64, last: u64, flags: u64) -> u64 {
    if flags & !1 != 0 {
        return errno(EINVAL);
    }
    const MAX_DESCRIPTORS: u64 = 256;
    let last = last.min(MAX_DESCRIPTORS - 1);
    if first > last {
        return 0;
    }
    let task = scheduler::current_task_id();
    let mut fd = first;
    while fd <= last {
        let _ = linux_fd::close(task, fd);
        fd += 1;
    }
    0
}

/// `flock(2)`. The guest compositors use an advisory lock on their Wayland
/// lockfile to detect a live socket owner. This kernel has no per-file lock
/// table yet, so the lock is granted: a second compositor then fails at
/// `bind` with EADDRINUSE and tries the next `wayland-N` name, which is
/// exactly what `wl_display_add_socket_auto` expects.
pub(super) fn linux_flock(fd: u64, operation: u64) -> u64 {
    const LOCK_SH: u64 = 1;
    const LOCK_EX: u64 = 2;
    const LOCK_NB: u64 = 4;
    const LOCK_UN: u64 = 8;
    if linux_fd::status_flags(scheduler::current_task_id(), fd).is_none() {
        return errno(EBADF);
    }
    match operation & !LOCK_NB {
        LOCK_SH | LOCK_EX | LOCK_UN => 0,
        _ => errno(EINVAL),
    }
}

pub(super) fn linux_write(fd: u64, buffer: u64, count: u64) -> u64 {
    finish(linux_fd::write(
        scheduler::current_task_id(),
        fd,
        buffer,
        count,
    ))
}

/// `writev(2)`: scatter output over up to 64 `struct iovec`s.
///
/// musl's stdio writes every stream through `writev`, not `write`; the old
/// implementation only accepted the console (`fd 1`/`2`) and returned
/// `EBADF` for files, so `openrc` silently produced a zero-byte `deptree`.
pub(super) fn linux_writev(fd: u64, iovec: u64, count: u64) -> u64 {
    const MAX_IOVECS: u64 = 64;
    if count > MAX_IOVECS {
        return errno(EINVAL);
    }
    let mut total = 0u64;
    for index in 0..count {
        let Some((base, mut length)) = iovec_at(iovec, index) else {
            return if total > 0 { total } else { errno(EFAULT) };
        };
        let mut address = base;
        while length > 0 {
            let written = linux_write(fd, address, length);
            if (written as i64) < 0 {
                return if total > 0 { total } else { written };
            }
            if written == 0 {
                return total;
            }
            total += written;
            address = address.wrapping_add(written);
            length -= written;
        }
    }
    total
}

/// `sendfile(2)`: copy between descriptors in 64 KiB kernel chunks.
///
/// busybox `cp` (and therefore the OpenRC/Alpine triggers) uses `sendfile`
/// when both ends are files; without it `cp` failed with `EFAULT` and the
/// glib schema copy in `boot-session` never ran.
pub(super) fn linux_sendfile(output: u64, input: u64, offset_ptr: u64, count: u64) -> u64 {
    let task = scheduler::current_task_id();
    let mut offset = if offset_ptr != 0 {
        match user_u64(offset_ptr) {
            Some(offset) => offset,
            None => return errno(EFAULT),
        }
    } else {
        match linux_fd::lseek(task, input, 0, SEEK_CUR) {
            Ok(offset) => offset,
            Err(code) => return errno(code),
        }
    };
    let mut total = 0u64;
    let mut buffer = alloc::vec![0_u8; 64 * 1024];
    while total < count {
        let want = (count - total).min(buffer.len() as u64) as usize;
        let read = match linux_fd::pread(task, input, offset, &mut buffer[..want]) {
            Ok(read) => read,
            Err(code) => return if total > 0 { total } else { errno(code) },
        };
        if read == 0 {
            break;
        }
        let mut written = 0usize;
        while written < read {
            let progress = match linux_fd::write_kernel(task, output, &buffer[written..read]) {
                Ok(progress) => progress,
                Err(code) => return if total > 0 { total } else { errno(code) },
            };
            if progress == 0 {
                break;
            }
            written += progress;
        }
        total += written as u64;
        offset += written as u64;
        if written < read {
            break;
        }
    }
    if offset_ptr != 0 {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&offset.to_le_bytes());
        let _ = user_memory::copy_to_user(&raw, offset_ptr);
    } else {
        // `pread` does not move the descriptor; the caller expects sendfile to.
        let _ = linux_fd::lseek(task, input, offset as i64, SEEK_SET);
    }
    total
}

/// `copy_file_range(2)`: the same chunked copy with independent offsets.
/// When an offset pointer is given the descriptor's own offset must not move,
/// so the output position is restored after the copy.
pub(super) fn linux_copy_file_range(
    input: u64,
    input_offset_ptr: u64,
    output: u64,
    output_offset_ptr: u64,
    length: u64,
    flags: u64,
) -> u64 {
    if flags != 0 {
        return errno(EINVAL);
    }
    let task = scheduler::current_task_id();
    let mut input_offset = if input_offset_ptr != 0 {
        match user_u64(input_offset_ptr) {
            Some(offset) => offset,
            None => return errno(EFAULT),
        }
    } else {
        match linux_fd::lseek(task, input, 0, SEEK_CUR) {
            Ok(offset) => offset,
            Err(code) => return errno(code),
        }
    };
    let mut saved_output_offset = None;
    let mut output_offset = 0u64;
    if output_offset_ptr != 0 {
        let Some(offset) = user_u64(output_offset_ptr) else {
            return errno(EFAULT);
        };
        output_offset = offset;
        saved_output_offset = linux_fd::lseek(task, output, 0, SEEK_CUR).ok().map(|offset| offset as i64);
        if let Err(code) = linux_fd::lseek(task, output, offset as i64, SEEK_SET) {
            return errno(code);
        }
    }
    let mut total = 0u64;
    let mut buffer = alloc::vec![0_u8; 64 * 1024];
    while total < length {
        let want = (length - total).min(buffer.len() as u64) as usize;
        let read = match linux_fd::pread(task, input, input_offset, &mut buffer[..want]) {
            Ok(read) => read,
            Err(code) => return if total > 0 { total } else { errno(code) },
        };
        if read == 0 {
            break;
        }
        let mut written = 0usize;
        while written < read {
            let progress = match linux_fd::write_kernel(task, output, &buffer[written..read]) {
                Ok(progress) => progress,
                Err(code) => return if total > 0 { total } else { errno(code) },
            };
            if progress == 0 {
                break;
            }
            written += progress;
        }
        total += written as u64;
        input_offset += written as u64;
        if written < read {
            break;
        }
    }
    if input_offset_ptr != 0 {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&input_offset.to_le_bytes());
        let _ = user_memory::copy_to_user(&raw, input_offset_ptr);
    } else {
        let _ = linux_fd::lseek(task, input, input_offset as i64, SEEK_SET);
    }
    if output_offset_ptr != 0 {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&(output_offset + total).to_le_bytes());
        let _ = user_memory::copy_to_user(&raw, output_offset_ptr);
        if let Some(original) = saved_output_offset {
            let _ = linux_fd::lseek(task, output, original as i64, SEEK_SET);
        }
    }
    total
}

/// `readv(2)`: gather input into up to 64 `struct iovec`s. musl's stdio
/// reads streams through `readv`, so without this `deptree_load` and every
/// other `FILE` read saw `ENOSYS`.
pub(super) fn linux_readv(fd: u64, iovec: u64, count: u64) -> u64 {
    const MAX_IOVECS: u64 = 64;
    if count > MAX_IOVECS {
        return errno(EINVAL);
    }
    let mut total = 0u64;
    for index in 0..count {
        let Some((base, length)) = iovec_at(iovec, index) else {
            return if total > 0 { total } else { errno(EFAULT) };
        };
        let mut done = 0u64;
        while done < length {
            let remaining = length - done;
            let read = finish(linux_fd::read(
                scheduler::current_task_id(),
                fd,
                base.wrapping_add(done),
                remaining,
            ));
            if (read as i64) < 0 {
                return if total > 0 { total } else { read };
            }
            if read == 0 {
                return total;
            }
            total += read;
            done += read;
            // A short read ends the call: the next descriptor must not block.
            if read < remaining {
                return total;
            }
        }
    }
    total
}

/// evdev capability ioctls for `/dev/input/eventN`. Returns `None` when
/// the request is not an `E`-type evdev command, so the caller can answer
/// `ENOTTY` like every other non-terminal descriptor.
fn input_ioctl(index: usize, request: u64, argument: u64) -> Option<u64> {
    // _IOC layout: dir 30..32, size 16..30, type 8..16, nr 0..8.
    if ((request >> 8) & 0xff) != 0x45 {
        return None;
    }
    let nr = (request & 0xff) as u8;
    let size = ((request >> 16) & 0x3fff) as usize;
    let copy_out = |bytes: &[u8]| -> u64 {
        if user_memory::copy_to_user(bytes, argument).is_err() {
            errno(EFAULT)
        } else {
            0
        }
    };
    Some(match nr {
        // EVIOCGVERSION -> EV_VERSION (0x010001).
        0x01 => {
            if size < 4 {
                return Some(errno(EINVAL));
            }
            copy_out(&0x0001_0001_u32.to_le_bytes())
        }
        // EVIOCGID: struct input_id { bustype, vendor, product, version }.
        0x02 => {
            if size < 8 {
                return Some(errno(EINVAL));
            }
            let mut id = [0_u8; 8];
            id[0..2].copy_from_slice(&0x06_u16.to_le_bytes()); // BUS_VIRTUAL
            id[2..4].copy_from_slice(&0x1af4_u16.to_le_bytes());
            id[4..6].copy_from_slice(&0x1052_u16.to_le_bytes());
            id[6..8].copy_from_slice(&1_u16.to_le_bytes());
            copy_out(&id)
        }
        // EVIOCGNAME(len): the name from the virtio device config.
        0x06 => {
            if size == 0 {
                return Some(errno(EINVAL));
            }
            let name = crate::virtio_input::name(index);
            // Keep one byte for the NUL terminator: never write past the
            // caller-supplied buffer size.
            let take = size.saturating_sub(1).min(name.len());
            let mut buffer = alloc::vec![0_u8; take + 1];
            buffer[..take].copy_from_slice(&name[..take]);
            if copy_out(&buffer) == 0 {
                (take + 1) as u64
            } else {
                errno(EFAULT)
            }
        }
        // EVIOCGPHYS: a stable bus/port-style path, one per slot.
        0x07 => {
            if size == 0 {
                return Some(errno(EINVAL));
            }
            let phys: &[u8] = if index == 0 {
                b"virtio0/input0"
            } else {
                b"virtio0/input1"
            };
            let take = size.saturating_sub(1).min(phys.len());
            let mut buffer = alloc::vec![0_u8; take + 1];
            buffer[..take].copy_from_slice(&phys[..take]);
            if copy_out(&buffer) == 0 {
                (take + 1) as u64
            } else {
                errno(EFAULT)
            }
        }
        // EVIOCGUNIQ: virtio-input has no unique id.
        0x08 => 0,
        // EVIOCGPROP: the VIRTIO_INPUT_PROP_* bitmap.
        0x09 => {
            let props = crate::virtio_input::props(index);
            let take = size.min(props.len());
            copy_out(&props[..take])
        }
        // EVIOCGKEY: current key state, tracked from EV_KEY events.
        0x18 => {
            let keys = crate::virtio_input::key_state(index);
            let take = size.min(keys.len());
            copy_out(&keys[..take])
        }
        // EVIOCGLED / EVIOCGSND / EVIOCGSW: no outputs or switches.
        0x19 | 0x1a | 0x1b => {
            let take = size.min(96);
            let zeros = alloc::vec![0_u8; take];
            copy_out(&zeros)
        }
        // EVIOCGBIT(ev, len): the capability bitmap for one event type.
        0x20..=0x3f => {
            let bits = crate::virtio_input::ev_bits(index, nr - 0x20);
            let take = size.min(bits.len());
            copy_out(&bits[..take])
        }
        // EVIOCGABS(abs): struct input_absinfo { value, min, max, fuzz, flat,
        // resolution }.
        0x40..=0x7f => {
            if size < 24 {
                return Some(errno(EINVAL));
            }
            let Some((value, minimum, maximum, fuzz, flat, resolution)) =
                crate::virtio_input::abs_info(index, nr - 0x40)
            else {
                return Some(errno(EINVAL));
            };
            let mut raw = [0_u8; 24];
            for (slot, word) in raw
                .chunks_exact_mut(4)
                .zip([value, minimum, maximum, fuzz, flat, resolution])
            {
                slot.copy_from_slice(&word.to_le_bytes());
            }
            copy_out(&raw)
        }
        _ => return Some(errno(ENOTTY)),
    })
}

/// `ioctl(2)`: only the terminal size query is meaningful while stdout is the
/// console. Everything else reports "not a tty".
pub(super) fn linux_ioctl(fd: u64, request: u64, argument: u64) -> u64 {
    if let Err(code) = linux_fd::ioctl_allowed(scheduler::current_task_id(), fd, request) {
        return errno(code);
    }
    const TIOCGWINSZ: u64 = 0x5413;
    const TCGETS: u64 = 0x5401;
    const FIONBIO: u64 = 0x5421;
    /// `_IO('W', 0x01)`: present the current `/dev/gpufb` contents.
    const GPU_IOCTL_FLUSH: u64 = 0x5701;
    /// udmabuf: turn a memfd into a dmabuf fd. The modern uapi renumbered
    /// UDMABUF_CREATE from 0x15 to 0x42 (24-byte struct udmabuf_create).
    const UDMABUF_CREATE: u64 = 0x4018_7542;
    /// dma-buf ioctls llvmpipe runs on its dummy dmabuf.
    const DMA_BUF_SYNC: u64 = 0x4008_6200;
    const DMA_BUF_EXPORT_SYNC_FILE: u64 = 0xC008_6202;
    let task = scheduler::current_task_id();
    if linux_fd::is_drm(task, fd) {
        if let Some(result) = crate::drm::ioctl(request, argument) {
            return result;
        }
        if let Some(result) = crate::drm::render_ioctl(request, argument) {
            return result;
        }
    }
    if let Some(index) = linux_fd::input_device(task, fd) {
        if let Some(result) = input_ioctl(index, request, argument) {
            return result;
        }
        return errno(ENOTTY);
    }
    if request == GPU_IOCTL_FLUSH && linux_fd::is_gpufb(task, fd) {
        return if crate::virtio_gpu::flush() {
            0
        } else {
            errno(EIO)
        };
    }
    // Rust's runtime (and socket2) set non-blocking through `ioctl(FIONBIO)`
    // on sockets and pipes. The fd is not a terminal, so the tty branch below
    // must not answer EBADF for it.
    if request == FIONBIO {
        if linux_fd::status_flags(task, fd).is_none() {
            return errno(EBADF);
        }
        let mut raw = [0_u8; 4];
        if user_memory::copy_from_user(argument, &mut raw).is_err() {
            return errno(EFAULT);
        }
        let enabled = u32::from_le_bytes(raw) != 0;
        return match linux_fd::set_nonblock(task, fd, enabled) {
            // A file-backed descriptor is always "ready"; treat the request
            // as a successful no-op like Linux does for regular files.
            Ok(()) => 0,
            Err(_) => 0,
        };
    }
    // Virtual-terminal ioctls for the synthetic VT console (`/dev/tty0`).
    // seatd drives every session activation through them; without answers the
    // seat never activates and clients cannot open devices.
    if linux_fd::is_console(task, fd) {
        const VT_OPENQRY: u64 = 0x5600;
        const VT_SETMODE: u64 = 0x5601;
        const VT_GETMODE: u64 = 0x5602;
        const VT_GETSTATE: u64 = 0x5603;
        const VT_SENDSIG: u64 = 0x5604;
        const VT_RELDISP: u64 = 0x5605;
        const VT_ACTIVATE: u64 = 0x5606;
        const VT_WAITACTIVE: u64 = 0x5607;
        const VT_DISALLOCATE: u64 = 0x5608;
        const KDSETMODE: u64 = 0x4B3A;
        const KDGETMODE: u64 = 0x4B3B;
        const TIOCSCTTY: u64 = 0x540E;
        match request {
            VT_OPENQRY => {
                let one = 1_u32.to_le_bytes();
                if user_memory::copy_to_user(&one, argument).is_err() {
                    return errno(EFAULT);
                }
                return 0;
            }
            VT_GETSTATE => {
                // struct vt_stat { unsigned short v_active, v_signal, v_state; }
                let mut stat = [0_u8; 6];
                stat[0..2].copy_from_slice(&1_u16.to_le_bytes());
                stat[4..6].copy_from_slice(&0x0002_u16.to_le_bytes());
                if user_memory::copy_to_user(&stat, argument).is_err() {
                    return errno(EFAULT);
                }
                return 0;
            }
            VT_GETMODE => {
                // struct vt_mode { char mode(VT_AUTO=0), waitv; short relsig, acksig, frsig; }
                let mode = [0_u8; 8];
                if user_memory::copy_to_user(&mode, argument).is_err() {
                    return errno(EFAULT);
                }
                return 0;
            }
            VT_SETMODE | VT_SENDSIG | VT_RELDISP | VT_ACTIVATE | VT_WAITACTIVE
            | VT_DISALLOCATE | KDSETMODE | KDGETMODE | TIOCSCTTY => return 0,
            _ => {}
        }
    }
    if linux_fd::is_udmabuf(task, fd) {
    }
    if request == UDMABUF_CREATE && linux_fd::is_udmabuf(task, fd) {
        // struct udmabuf_create { u32 memfd, u32 flags, u64 offset, u64 size }
        let mut raw = [0_u8; 24];
        if user_memory::copy_from_user(argument, &mut raw).is_err() {
            return errno(EFAULT);
        }
        let memfd = u64::from(raw[0])
            | u64::from(raw[1]) << 8
            | u64::from(raw[2]) << 16
            | u64::from(raw[3]) << 24;
        let size = u64::from(raw[16])
            | u64::from(raw[17]) << 8
            | u64::from(raw[18]) << 16
            | u64::from(raw[19]) << 24
            | u64::from(raw[20]) << 32
            | u64::from(raw[21]) << 40
            | u64::from(raw[22]) << 48
            | u64::from(raw[23]) << 56;
        let result = linux_fd::udmabuf_create(task, memfd, 0, size);
        match &result {
            Ok(fd) => crate::console::write_dec_u64(*fd),
            Err(code) => crate::console::write_dec_u64(*code),
        }
        crate::console::write_str("\r\n");
        return finish(result);
    }
    if linux_fd::is_memfd(task, fd) {
        if request == DMA_BUF_EXPORT_SYNC_FILE {
            // struct dma_buf_export_sync_file { u32 flags, s32 fd }: the fence
            // completes on the CPU, so the exported sync file is signalled.
            let Ok(sync_fd) = linux_fd::eventfd(task, 1, true) else {
                return errno(ENOMEM);
            };
            let mut out = [0_u8; 8];
            out[4..8].copy_from_slice(&(sync_fd as i32).to_le_bytes());
            if user_memory::copy_to_user(&out, argument).is_err() {
                return errno(EFAULT);
            }
            return 0;
        }
        if request == DMA_BUF_SYNC {
            // CPU-side memory: the read/write barriers are no-ops here.
            return 0;
        }
    }
    if request == TCGETS {
        // glibc's isatty() probes TCGETS; a console answers with a zeroed
        // termios so isatty returns true.
        if linux_fd::is_console(task, fd) {
            let termios = [0_u8; 60];
            if user_memory::copy_to_user(&termios, argument).is_err() {
                return errno(EFAULT);
            }
            return 0;
        }
    }
    // Everything that reached here is not a device with a specific handler.
    // The POSIX answer for an OPEN descriptor is ENOTTY ("inappropriate
    // ioctl for device"); EBADF only means the descriptor is not open.
    // libsystemd's isatty_safe asserts on EBADF for fds above stdio - the
    // deniald startup died on exactly that distinction.
    if linux_fd::status_flags(task, fd).is_none() {
        return errno(EBADF);
    }
    if request != TIOCGWINSZ {
        return errno(ENOTTY);
    }
    if !linux_fd::is_console(task, fd) {
        return errno(ENOTTY);
    }
    // struct winsize { unsigned short ws_row, ws_col, ws_xpixel, ws_ypixel }
    let mut size = [0_u8; 8];
    size[0..2].copy_from_slice(&24_u16.to_le_bytes());
    size[2..4].copy_from_slice(&80_u16.to_le_bytes());
    if user_memory::copy_to_user(&size, argument).is_err() {
        return errno(EFAULT);
    }
    0
}

/// `openat(2)`: delegates to the process fd table, which also recognizes the
/// synthetic `/dev` nodes before touching the VFS.
pub(super) fn linux_openat(dirfd: u64, path_ptr: u64, flags: u64, _mode: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    if path.is_empty() {
        return errno(ENOENT);
    }
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    finish(linux_fd::open(
        scheduler::current_task_id(),
        &cwd,
        &path,
        flags,
    ))
}

/// `faccessat(2)`: existence/readability probe. The Linux view is read-only,
/// so a write-access check fails with `-EROFS`.
pub(super) fn linux_faccessat(dirfd: u64, path_ptr: u64, mode: u64) -> u64 {
    const W_OK: u64 = 2;
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    if path.is_empty() {
        return errno(ENOENT);
    }
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    let Ok(resolved) = resolve(&cwd, &path) else {
        return errno(ENOENT);
    };
    if crate::sysfs::lookup(&resolved).is_some()
        || crate::procfs::lookup(&resolved).is_some()
        || linux_fd::device_exists(&resolved)
    {
        // Synthetic nodes are read-only; a write probe reports EROFS like the
        // rest of the image.
        return if mode & W_OK != 0 { errno(EROFS) } else { 0 };
    }
    // Unlisted /proc and /sys nodes do not exist: probing libraries expect
    // the clean ENOENT, not the VFS read-path EACCES.
    if resolved == "/proc" || resolved.starts_with("/proc/") || resolved.starts_with("/sys/") {
        return errno(ENOENT);
    }
    if !scheduler::current_read_path_allowed(&resolved) {
        return errno(EACCES);
    }
    if mode & W_OK != 0 && !crate::scheduler::linux_write_allowed(&resolved) {
        // SFS is read-only; the Linux scratch tree (`/tmp`) is the one place
        // a toolkit may create caches. A blanket EROFS made fontconfig
        // declare every cache directory unwritable.
        return errno(EROFS);
    }
    // Only the nodes the kernel actually exposes: a blanket "/dev exists"
    // made libdrm's `drmGetMinorType` see card128 and classify the render
    // node as a primary one.
    if linux_fd::device_exists(&resolved)
        || crate::vfs::file_len(&cwd, &resolved).is_ok()
        || crate::vfs::resolve_directory(&cwd, &resolved).is_ok()
    {
        0
    } else {
        errno(ENOENT)
    }
}

/// `newfstatat(2)`: stat by path, or by descriptor with `AT_EMPTY_PATH`.
pub(super) fn linux_newfstatat(dirfd: u64, path_ptr: u64, buffer: u64, flags: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    let task = scheduler::current_task_id();
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return errno(ENOENT);
        }
        return finish(linux_fd::fstat(task, dirfd, buffer).map(|_| 0));
    }
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    let Ok(resolved) = resolve(&cwd, &path) else {
        return errno(ENOENT);
    };
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    match stat_resolved(&cwd, &path, &resolved, follow) {
        Ok((mode, size, ino, Some(rdev))) => {
            finish(linux_fd::write_stat_rdev(buffer, mode, size, ino, rdev).map(|_| 0))
        }
        Ok((mode, size, ino, None)) => {
            finish(linux_fd::write_stat(buffer, mode, size, ino).map(|_| 0))
        }
        Err(code) => errno(code),
    }
}

/// Shared path stat for `newfstatat` and `statx`.
pub(super) fn stat_resolved(
    cwd: &str,
    path: &str,
    resolved: &str,
    follow: bool,
) -> Result<(u32, u64, u64, Option<u64>), u64> {
    if let Some(node) = crate::sysfs::lookup(resolved) {
        let (mode, size) = match node {
            crate::sysfs::Node::File(data) => (S_IFREG | 0o444, data.len() as u64),
            crate::sysfs::Node::Dir => (S_IFDIR | 0o755, 0),
            crate::sysfs::Node::Symlink(target) => (S_IFLNK | 0o777, target.len() as u64),
        };
        return Ok((mode, size, linux_fd::path_ino(resolved), None));
    }
    if let Some(node) = crate::procfs::lookup(resolved) {
        let (mode, size) = match node {
            crate::procfs::Node::File(data) => (S_IFREG | 0o444, data.len() as u64),
            crate::procfs::Node::Dir(names) => (S_IFDIR | 0o555, names.len() as u64),
            crate::procfs::Node::Symlink(target) => (S_IFLNK | 0o777, target.len() as u64),
        };
        return Ok((mode, size, linux_fd::path_ino(resolved), None));
    }
    if let Some((mode, rdev)) = synthetic_device_stat(resolved) {
        return Ok((mode, 0, linux_fd::path_ino(resolved), Some(rdev)));
    }
    if !scheduler::current_read_path_allowed(resolved) {
        return Err(EACCES);
    }
    // `lstat`/`AT_SYMLINK_NOFOLLOW` reports the link itself. SFS stores the
    // target in the inode; following it is the default `stat` path below.
    if !follow && crate::vfs::lstat_kind("/", resolved).ok() == Some(3) {
        let length = crate::vfs::link_len("/", resolved).unwrap_or(0) as u64;
        return Ok((S_IFLNK | 0o777, length, linux_fd::path_ino(resolved), None));
    }
    // Directories first: the SFS backend reports a directory inode's size as
    // 0, so `file_len` succeeds for one and used to classify it as a regular
    // file. Toolkits stat XDG/data directories and reject a "file" where a
    // directory is required (xkbcommon refused its own config root).
    if crate::vfs::read_dir("/", resolved).is_ok() {
        return Ok((S_IFDIR | 0o755, 0, linux_fd::path_ino(resolved), None));
    }
    if let Ok(size) = crate::vfs::file_len("/", resolved) {
        let file_mode = if crate::scheduler::linux_ram_path(resolved) {
            S_IFREG | 0o644
        } else {
            // SFS stores no permission bits and is read-only; report files
            // executable so gio's g_find_program_in_path() accepts /sfs.
            S_IFREG | 0o555
        };
        return Ok((file_mode, size as u64, linux_fd::path_ino(resolved), None));
    }
    // Bound AF_UNIX sockets live only in the kernel registry; deniald stats
    // its portal socket right after binding it.
    if linux_fd::listener_exists(resolved.as_bytes()) {
        return Ok((S_IFSOCK | 0o600, 0, linux_fd::path_ino(resolved), None));
    }
    Err(ENOENT)
}

const SYSFS_MAGIC: i64 = 0x6265_6572;
const PROC_SUPER_MAGIC: i64 = 0x0000_9fa0;
const SFS_MAGIC: i64 = 0x5346_5331;
const TMPFS_MAGIC: i64 = 0x0102_1994;

/// Writes a 120-byte AArch64 struct statfs. Only the filesystem type is
/// consulted by systemd/libudev; the rest keeps statfs(1) and df sane.
fn write_statfs(buffer: u64, magic: i64) -> u64 {
    let mut raw = [0_u8; 120];
    raw[0..8].copy_from_slice(&magic.to_le_bytes());
    raw[8..16].copy_from_slice(&4096_i64.to_le_bytes());
    raw[16..24].copy_from_slice(&1024_i64.to_le_bytes());
    raw[64..72].copy_from_slice(&255_i64.to_le_bytes());
    raw[72..80].copy_from_slice(&4096_i64.to_le_bytes());
    match user_memory::copy_to_user(&raw, buffer) {
        Ok(()) => 0,
        Err(_) => errno(EFAULT),
    }
}

/// statfs(2). libudev checks that a syspath is backed by SYSFS_MAGIC before
/// accepting it as a device, so the synthetic tree must answer for itself.
pub(super) fn linux_statfs(path_ptr: u64, buffer: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    let cwd = match dirfd_cwd(AT_FDCWD as u64) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    let Ok(resolved) = resolve(&cwd, &path) else {
        return errno(ENOENT);
    };
    let magic = if crate::sysfs::lookup(&resolved).is_some() {
        SYSFS_MAGIC
    } else if crate::procfs::lookup(&resolved).is_some() {
        PROC_SUPER_MAGIC
    } else if resolved == "/dev" || resolved.starts_with("/dev/") || resolved == "/tmp"
        || resolved.starts_with("/tmp/")
    {
        TMPFS_MAGIC
    } else {
        SFS_MAGIC
    };
    write_statfs(buffer, magic)
}

/// fstatfs(2): same magic, chosen from the descriptor target.
pub(super) fn linux_fstatfs(fd: u64, buffer: u64) -> u64 {
    let task = scheduler::current_task_id();
    if let Err(code) = linux_fd::require_right(task, fd, linux_fd::FD_FSTAT) {
        return errno(code);
    }
    match linux_fd::fs_magic(task, fd) {
        Some(magic) => write_statfs(buffer, magic),
        None => errno(EBADF),
    }
}

/// `statx(2)`: glib and wlroots probe it first; without it they fall back to
/// repeated retry loops. Writes the fixed 256-byte `struct statx`.
pub(super) fn linux_statx(dirfd: u64, path_ptr: u64, flags: u64, _mask: u64, buffer: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    let task = scheduler::current_task_id();
    let fields = if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return errno(ENOENT);
        }
        match linux_fd::stat_fields(task, dirfd) {
            Ok(fields) => fields,
            Err(code) => return errno(code),
        }
    } else {
        let cwd = match dirfd_cwd(dirfd) {
            Ok(cwd) => cwd,
            Err(code) => return code,
        };
        let Ok(resolved) = resolve(&cwd, &path) else {
            return errno(ENOENT);
        };
        match stat_resolved(&cwd, &path, &resolved, flags & AT_SYMLINK_NOFOLLOW == 0) {
            Ok(fields) => fields,
            Err(code) => return errno(code),
        }
    };
    match write_statx(buffer, fields.0, fields.1, fields.2, fields.3) {
        Ok(()) => 0,
        Err(code) => errno(code),
    }
}

pub(super) fn write_statx(
    buffer: u64,
    mode: u32,
    size: u64,
    ino: u64,
    rdev: Option<u64>,
) -> Result<(), u64> {
    const STATX_BASIC_STATS: u32 = 0x7ff;
    // systemd's fds_are_same_mount() statx's fds with STATX_MNT_ID and
    // reads the fallback path when the mask bit is missing; that fallback
    // asserts on this kernel. STATX_MNT_ID is bit 12 (0x1000), not the
    // neighbouring STATX_SUBVOL bit.
    const STATX_MNT_ID: u32 = 0x1000;
    let mut raw = [0_u8; 256];
    raw[0..4].copy_from_slice(&(STATX_BASIC_STATS | STATX_MNT_ID).to_le_bytes());
    raw[4..8].copy_from_slice(&4096_u32.to_le_bytes());
    raw[16..20].copy_from_slice(&1_u32.to_le_bytes());
    raw[28..30].copy_from_slice(&(mode as u16).to_le_bytes());
    raw[32..40].copy_from_slice(&ino.to_le_bytes());
    raw[40..48].copy_from_slice(&size.to_le_bytes());
    raw[48..56].copy_from_slice(&(size / 512).to_le_bytes());
    if let Some(device) = rdev {
        raw[128..132].copy_from_slice(&linux_fd::major(device).to_le_bytes());
        raw[132..136].copy_from_slice(&linux_fd::minor(device).to_le_bytes());
    }
    // One mount id for the whole synthetic tree: same-mount comparisons see
    // every descriptor as shared, which matches the single-filesystem view.
    raw[144..152].copy_from_slice(&1_u64.to_le_bytes());
    user_memory::copy_to_user(&raw, buffer).map_err(|_| EFAULT)
}

/// Mode and `st_rdev` for a synthetic `/dev` node.
pub(super) fn synthetic_device_stat(path: &str) -> Option<(u32, u64)> {
    const S_IFCHR: u32 = 0o020000;
    match path {
        // Synthetic directory roots (udev enumerate stats and scans them).
        "/dev" | "/dev/input" => Some((S_IFDIR | 0o755, 0)),
        "/dev/dri" if crate::virtio_gpu::size().is_some() => Some((S_IFDIR | 0o755, 0)),
        "/dev/dri/card0" if crate::virtio_gpu::size().is_some() => Some((
            S_IFCHR | 0o666,
            linux_fd::makedev(crate::sysfs::DRM_MAJOR, crate::sysfs::CARD_MINOR),
        )),
        "/dev/dri/renderD128" if crate::virtio_gpu::size().is_some() => Some((
            S_IFCHR | 0o666,
            linux_fd::makedev(crate::sysfs::DRM_MAJOR, crate::sysfs::RENDER_MINOR),
        )),
        _ if linux_fd::device_exists(path) => Some((S_IFCHR | 0o666, 0)),
        _ => None,
    }
}

/// `readlinkat(2)`: the synthetic sysfs symlinks (`device/subsystem`).
pub(super) fn linux_readlinkat(dirfd: u64, path_ptr: u64, buffer: u64, size: u64) -> u64 {
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    // readlinkat(fd, "") reads the link an O_PATH descriptor points at.
    if path.is_empty() {
        let dirfd = normalized_dirfd(dirfd);
        if dirfd == AT_FDCWD {
            return errno(ENOENT);
        }
        let Some(target) = linux_fd::symlink_target(scheduler::current_task_id(), dirfd as u64) else {
            return errno(EINVAL);
        };
        let bytes = target.as_bytes();
        let take = bytes.len().min(size as usize);
        if user_memory::copy_to_user(&bytes[..take], buffer).is_err() {
            return errno(EFAULT);
        }
        return take as u64;
    }
    let cwd = match dirfd_cwd(dirfd) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };
    let Ok(resolved) = resolve(&cwd, &path) else {
        return errno(ENOENT);
    };
    let target = if let Some(node) = crate::sysfs::lookup(&resolved) {
        match node {
            crate::sysfs::Node::Symlink(target) => String::from(target),
            _ => return errno(EINVAL),
        }
    } else if let Some(node) = crate::procfs::lookup(&resolved) {
        match node {
            crate::procfs::Node::Symlink(target) => String::from(target),
            _ => return errno(EINVAL),
        }
    } else {
        match crate::vfs::read_link("/", &resolved) {
            Ok(target) => target,
            Err(()) => {
                // A missing path is ENOENT on Linux; EINVAL is reserved for
                // an existing node that is not a symlink. libudev probes
                // optional class links such as /sys/class/input/subsystem.
                let exists = crate::vfs::read_dir("/", &resolved).is_ok()
                    || crate::vfs::file_len("/", &resolved).is_ok();
                return errno(if exists { EINVAL } else { ENOENT });
            }
        }
    };
    let bytes = target.as_bytes();
    let take = bytes.len().min(size as usize);
    if user_memory::copy_to_user(&bytes[..take], buffer).is_err() {
        return errno(EFAULT);
    }
    take as u64
}

/// `fcntl(2)`: descriptor duplication plus the flags queries every libc probes.
pub(super) fn linux_fcntl(fd: u64, command: u64, argument: u64) -> u64 {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_DUPFD_CLOEXEC: u64 = 1030;
    const F_ADD_SEALS: u64 = 1033;
    const F_GET_SEALS: u64 = 1034;
    const EPERM: u64 = 1;
    let task = scheduler::current_task_id();
    match command {
        F_DUPFD => finish(linux_fd::dup_from(task, fd, argument, false)),
        F_DUPFD_CLOEXEC => finish(linux_fd::dup_from(task, fd, argument, true)),
        F_GETFD => match linux_fd::get_cloexec(task, fd) {
            Ok(true) => 1,
            Ok(false) => 0,
            Err(code) => errno(code),
        },
        F_GETFL => match linux_fd::status_flags(task, fd) {
            Some(flags) => flags,
            None => errno(EBADF),
        },
        F_SETFD => finish(linux_fd::set_cloexec(task, fd, argument & 1 != 0).map(|_| 0)),
        F_SETFL => {
            let _ = linux_fd::set_nonblock(task, fd, argument & 0x800 != 0);
            0
        }
        F_ADD_SEALS => {
            let result = linux_fd::memfd_add_seals(task, fd, argument as u32).map(|_| 0);
            if result.is_err() {
                crate::console::write_str("F_ADD_SEALS fd=");
                crate::console::write_dec_u64(fd);
                crate::console::write_str(" failed\r\n");
            }
            finish(result)
        }
        F_GET_SEALS => match linux_fd::memfd_get_seals(task, fd) {
            Ok(seals) => u64::from(seals),
            Err(code) => errno(code),
        },
        _ => errno(EINVAL),
    }
}

pub(super) fn linux_getcwd(buffer: u64, capacity: u64) -> u64 {
    let cwd = display_path(&scheduler::current_task_cwd());
    let bytes = cwd.as_bytes();
    if bytes.len() + 1 > capacity as usize {
        return errno(ERANGE);
    }
    if user_memory::copy_to_user(bytes, buffer).is_err() {
        return errno(EFAULT);
    }
    let nul = [0_u8; 1];
    if user_memory::copy_to_user(&nul, buffer + bytes.len() as u64).is_err() {
        return errno(EFAULT);
    }
    (bytes.len() + 1) as u64
}

pub(super) fn scratch_resolved(dirfd: u64, path_pointer: u64) -> Result<(String, String), u64> {
    let Some(path) = user_string(path_pointer) else {
        return Err(EFAULT);
    };
    if path.is_empty() {
        return Err(ENOENT);
    }
    // `scratch_resolved` carries positive errno values to its callers, while
    // `dirfd_cwd` returns the syscall-style negative form.
    let cwd = dirfd_cwd(dirfd).map_err(|code| (code as i64).wrapping_neg() as u64)?;
    let resolved = resolve(&cwd, &path).map_err(|_| ENOENT)?;
    if !crate::scheduler::linux_write_allowed(&resolved) {
        return Err(EROFS);
    }
    // The mapped absolute path is what the VFS mutation must use: a relative
    // name under `/run` lives at `/sfs/run` in the kernel tree.
    Ok((String::from("/"), resolved))
}

pub(super) fn linux_mkdirat(dirfd: u64, path_pointer: u64, _mode: u64) -> u64 {
    let (cwd, path) = match scratch_resolved(dirfd, path_pointer) {
        Ok(pair) => pair,
        Err(code) => return errno(code),
    };
    match crate::vfs::mkdir(&cwd, &path) {
        Ok(()) => 0,
        Err(()) => {
            if crate::vfs::resolve_directory(&cwd, &path).is_ok() {
                errno(EEXIST)
            } else {
                // POSIX: a missing parent is ENOENT, not EACCES. Rust's
                // create_dir_all only recurses on ENOENT - reporting EACCES
                // here aborts xcursor_sentinel's theme-directory setup.
                let parent = match path.rsplit_once('/') {
                    Some(("", _)) => "/",
                    Some((base, _)) => base,
                    None => "/",
                };
                if crate::vfs::resolve_directory(&cwd, parent).is_ok() {
                    errno(EACCES)
                } else {
                    errno(ENOENT)
                }
            }
        }
    }
}

pub(super) fn linux_unlinkat(dirfd: u64, path_pointer: u64, flags: u64) -> u64 {
    if flags & !AT_REMOVEDIR != 0 {
        return errno(EINVAL);
    }
    // POSIX shm lives in the synthetic /dev/shm namespace, not in the
    // scratch VFS; wlroots calls shm_unlink() on every buffer it allocates.
    if let Some(path) = user_string(path_pointer) {
        if let Ok(cwd) = dirfd_cwd(dirfd) {
            if let Ok(resolved) = resolve(&cwd, &path) {
                if let Some(name) = resolved.strip_prefix("/dev/shm/") {
                    return match linux_fd::shm_unlink(name) {
                        Ok(()) => 0,
                        Err(code) => errno(code),
                    };
                }
                // A stale bound socket node: the registry is the only state.
                if linux_fd::listener_unlink(resolved.as_bytes()) {
                    return 0;
                }
            }
        }
    }
    let (cwd, path) = match scratch_resolved(dirfd, path_pointer) {
        Ok(pair) => pair,
        Err(code) => return errno(code),
    };
    let result = if flags & AT_REMOVEDIR != 0 {
        crate::vfs::rmdir(&cwd, &path, scheduler::cwd_in_use)
    } else {
        crate::vfs::unlink(&cwd, &path)
    };
    if result.is_ok() {
        0
    } else {
        // A failed removal here means the target is missing (the write
        // permission was already checked by scratch_resolved); callers such
        // as OpenRC's shutdown loop expect the POSIX ENOENT.
        errno(ENOENT)
    }
}

pub(super) fn linux_renameat(
    old_dirfd: u64,
    old_pointer: u64,
    new_dirfd: u64,
    new_pointer: u64,
    flags: u64,
) -> u64 {
    if flags & !(RENAME_NOREPLACE | 2) != 0 || flags & 2 != 0 {
        return errno(ENOSYS);
    }
    let (_, old_path) = match scratch_resolved(old_dirfd, old_pointer) {
        Ok(pair) => pair,
        Err(code) => return errno(code),
    };
    let (_, new_path) = match scratch_resolved(new_dirfd, new_pointer) {
        Ok(pair) => pair,
        Err(code) => return errno(code),
    };
    let result = if flags & RENAME_NOREPLACE != 0 {
        crate::vfs::rename("/", &old_path, &new_path, scheduler::cwd_in_use)
    } else {
        crate::vfs::replace("/", &old_path, &new_path)
    };
    if result.is_ok() {
        0
    } else {
        errno(EACCES)
    }
}

pub(super) fn linux_chdir(path_ptr: u64) -> u64 {
    if scheduler::current_task_capability_mode() {
        return errno(EPERM);
    }
    let Some(path) = user_string(path_ptr) else {
        return errno(EFAULT);
    };
    let cwd = scheduler::current_task_cwd();
    let Ok(resolved) = resolve_directory(&cwd, &path) else {
        return errno(ENOENT);
    };
    if !scheduler::current_read_path_allowed(&resolved) {
        return errno(EACCES);
    }
    if scheduler::set_current_task_cwd(resolved) {
        0
    } else {
        errno(EACCES)
    }
}

/// glibc wrappers that take an `int` dirfd pass a zero-extended 32-bit
/// value, while direct syscalls sign-extend it: normalize both spellings
/// before comparing with AT_FDCWD.
fn normalized_dirfd(dirfd: u64) -> i64 {
    (dirfd as u32 as i32) as i64
}

/// Resolves `dirfd` to a working directory: `AT_FDCWD` uses the process cwd,
/// a directory descriptor uses its own path, anything else is `EBADF`.
pub(super) fn dirfd_cwd(dirfd: u64) -> Result<alloc::string::String, u64> {
    let dirfd = normalized_dirfd(dirfd);
    if dirfd == AT_FDCWD {
        if scheduler::current_task_capability_mode() {
            return Err(errno(EPERM));
        }
        return Ok(scheduler::current_task_cwd());
    }
    linux_fd::directory_path(scheduler::current_task_id(), dirfd as u64)
        .ok_or_else(|| errno(EBADF))
}

/// Copies a NUL-terminated user string (bounded to 255 bytes).
pub(super) fn user_string(pointer: u64) -> Option<alloc::string::String> {
    let mut bytes = alloc::vec::Vec::new();
    for index in 0..4096u64 {
        let mut byte = [0_u8; 1];
        user_memory::copy_from_user(pointer.wrapping_add(index), &mut byte).ok()?;
        if byte[0] == 0 {
            break;
        }
        bytes.push(byte[0]);
    }
    core::str::from_utf8(&bytes)
        .ok()
        .map(alloc::string::String::from)
}

/// Copies a `char *const argv[]`/`envp[]` array: up to 64 NUL-terminated
/// strings, bounded like `user_string`. A null pointer means an empty array.
pub(super) fn user_string_array(pointer: u64) -> Option<Vec<Vec<u8>>> {
    /// Linux allows large environments; the old 64-entry cap made `execve`
    /// fail with `EFAULT` for any command whose environment grew past it
    /// (busybox `cp` after OpenRC exported its variables).
    const MAX_ENTRIES: u64 = 1024;
    /// Total bytes of all strings, so a hostile array cannot exhaust the heap.
    const MAX_BYTES: usize = 256 * 1024;
    let mut result = Vec::new();
    let mut total = 0usize;
    if pointer == 0 {
        return Some(result);
    }
    for index in 0..MAX_ENTRIES {
        let mut raw = [0_u8; 8];
        user_memory::copy_from_user(pointer.wrapping_add(index * 8), &mut raw).ok()?;
        let entry = u64::from_le_bytes(raw);
        if entry == 0 {
            return Some(result);
        }
        let text = user_string(entry)?.into_bytes();
        total = total.checked_add(text.len() + 1)?;
        if total > MAX_BYTES {
            return None;
        }
        result.push(text);
    }
    None
}

pub(super) fn linux_pread(fd: u64, buffer: u64, count: u64, offset: u64) -> u64 {
    let count = (count as usize).min(1 << 20);
    let mut bytes = alloc::vec![0_u8; count];
    let task = scheduler::current_task_id();
    match linux_fd::pread(task, fd, offset, &mut bytes) {
        Ok(read) => {
            if user_memory::copy_to_user(&bytes[..read], buffer).is_err() {
                return errno(EFAULT);
            }
            read as u64
        }
        Err(code) => errno(code),
    }
}

pub(super) fn linux_pwrite(fd: u64, buffer: u64, count: u64, offset: u64) -> u64 {
    let count = (count as usize).min(1 << 20);
    let mut bytes = alloc::vec![0_u8; count];
    if user_memory::copy_from_user(buffer, &mut bytes).is_err() {
        return errno(EFAULT);
    }
    match linux_fd::pwrite(scheduler::current_task_id(), fd, offset, &bytes) {
        Ok(written) => written as u64,
        Err(code) => errno(code),
    }
}

/// `mount(2)`. The kernel has no mountable block filesystems in the Linux
/// view: `proc`/`sysfs`/`devtmpfs`/`cgroup*` are synthetic and answered from
/// the kernel tables, and `tmpfs` becomes a `ramfs` backend inside the
/// writable service subtrees. Flags are accepted and ignored.
pub(super) fn linux_mount(source: u64, target: u64, filesystem: u64, _flags: u64) -> u64 {
    if scheduler::current_task_capability_mode() {
        return errno(EPERM);
    }
    let _ = source;
    let Some(target) = user_string(target) else {
        return errno(EFAULT);
    };
    let filesystem = if filesystem == 0 {
        String::new()
    } else {
        match user_string(filesystem) {
            Some(name) => name,
            None => return errno(EFAULT),
        }
    };
    match filesystem.as_str() {
        "proc" | "sysfs" | "devtmpfs" | "devpts" | "cgroup" | "cgroup2" | "securityfs"
        | "debugfs" | "tracefs" | "mqueue" | "efivarfs" | "fusectl" | "configfs" => 0,
        "tmpfs" | "ramfs" | "" => {
            let Ok(resolved) = resolve("/", &target) else {
                return errno(ENOENT);
            };
            if !scheduler::linux_write_allowed(&resolved) {
                return errno(EACCES);
            }
            if crate::vfs::mount("/", &resolved, "ramfs", "", 0, |_| false).is_ok() {
                return 0;
            }
            // An existing mount is success; a missing directory is ENOENT.
            match crate::vfs::resolve_directory("/", &resolved) {
                Ok(_) => 0,
                Err(()) => errno(ENOENT),
            }
        }
        _ => errno(EINVAL),
    }
}

/// `umount2(2)`: synthetic mounts are permanent and the kernel never wants a
/// Linux task to detach the root, so this is an idempotent success for a path
/// that resolves and `ENOENT` otherwise.
pub(super) fn linux_umount(target: u64, _flags: u64) -> u64 {
    if scheduler::current_task_capability_mode() {
        return errno(EPERM);
    }
    let Some(target) = user_string(target) else {
        return errno(EFAULT);
    };
    match resolve("/", &target) {
        Ok(_) => 0,
        Err(()) => errno(ENOENT),
    }
}

/// Linux `sysinfo(2)`. The booting init only needs a plausible structure,
/// but reporting real frame counts makes it useful to tests and health
/// tooling instead of a zeroed lie. `mem_unit` stays 1, so all sizes are
/// bytes. `sharedram`, swap and high memory are zero: the kernel has no
/// swap and keeps all managed RAM in one zone.
#[repr(C)]
struct LinuxSysinfo {
    uptime: i64,
    loads: [u64; 3],
    totalram: u64,
    freeram: u64,
    sharedram: u64,
    bufferram: u64,
    totalswap: u64,
    freeswap: u64,
    procs: u16,
    pad: u16,
    pad2: u32,
    totalhigh: u64,
    freehigh: u64,
    mem_unit: u32,
}

const _: () = assert!(core::mem::size_of::<LinuxSysinfo>() == 112);

pub(super) fn linux_sysinfo(pointer: u64) -> u64 {
    if pointer == 0 {
        return errno(EFAULT);
    }
    let page_size = crate::memory::PAGE_SIZE as u64;
    let total = crate::frame_allocator::total_pages() as u64 * page_size;
    let free = crate::frame_allocator::free_pages() as u64 * page_size;
    let info = LinuxSysinfo {
        uptime: 0,
        loads: [0; 3],
        totalram: total,
        freeram: free,
        sharedram: 0,
        bufferram: 0,
        totalswap: 0,
        freeswap: 0,
        procs: 0,
        pad: 0,
        pad2: 0,
        totalhigh: 0,
        freehigh: 0,
        mem_unit: 1,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&info as *const LinuxSysinfo).cast::<u8>(),
            core::mem::size_of::<LinuxSysinfo>(),
        )
    };
    match user_memory::copy_to_user(bytes, pointer) {
        Ok(()) => 0,
        Err(_) => errno(EFAULT),
    }
}

/// Zero-fills a user structure for the calls whose values do not matter to a
/// booting init (`getrusage`, `statfs`). A null pointer is a no-op, matching
/// the kernel's tolerance for optional outputs.
pub(super) fn linux_zero_struct(pointer: u64, length: usize) -> u64 {
    if pointer == 0 {
        return 0;
    }
    let bytes = alloc::vec![0_u8; length];
    if user_memory::copy_to_user(&bytes, pointer).is_err() {
        return errno(EFAULT);
    }
    0
}
