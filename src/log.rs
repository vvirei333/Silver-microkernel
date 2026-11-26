//! Structured kernel log with a console mirror.
//!
//! Every kernel byte written through [`crate::console`] is copied into a
//! 16 KiB circular buffer before it reaches the UART, so `dmesg` shows the
//! complete boot log — old direct prints included — not just lines that opted
//! into the logger. EL0 `print`/`write` uses `console::write_bytes_unlogged`,
//! so a userspace screen clear cannot erase the dump it just requested.
//! Structured entries add the context a raw boot line lacks:
//!
//! `[   12.345] INFO  sched    src/scheduler/switch.rs:123  switch task=7 (why: timer)`
//!
//! * when  — milliseconds since `boot_metrics::mark_boot_start` (the free-running
//!   architectural counter, so entries before the 100 Hz timer works are exact).
//! * how   — level (`ERROR`..`TRACE`) and the subsystem target.
//! * where — source file and line captured by the macro, not by hand.
//! * what  — the `format_args!` payload.
//! * why   — the optional reason passed to the `*_why!` macros.
//!
//! The buffer keeps the newest [`CAPACITY`] bytes after they leave the UART,
//! so a hang or panic still has history to show. `dmesg` reads it through
//! `SYS_LOG_DUMP`; [`dump_tail`] prints it from the panic path.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Severity, ordered from most to least urgent.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }

    const fn value(self) -> usize {
        self as usize
    }
}

/// Default threshold: INFO and above reach the UART; DEBUG/TRACE stay
/// available to code that opts in through [`set_level`].
static THRESHOLD: AtomicUsize = AtomicUsize::new(Level::Info as usize);

pub fn set_level(level: Level) {
    THRESHOLD.store(level.value(), Ordering::Release);
}

pub fn level() -> u8 {
    match THRESHOLD.load(Ordering::Acquire) {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        _ => 4,
    }
}

pub fn enabled(level: Level) -> bool {
    level.value() <= THRESHOLD.load(Ordering::Acquire)
}

const CAPACITY: usize = 16 * 1024;
const LINE_CAPACITY: usize = 192;

static LOCK: AtomicBool = AtomicBool::new(false);
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static mut CONSOLE_BUFFER: [u8; CAPACITY] = [0; CAPACITY];

/// Best-effort ring guard. Interrupts are masked while the index moves, so an
/// IRQ that logs cannot observe half a line; the UART lock is never held.
struct Guard {
    daif: u64,
}

fn acquire() -> Option<Guard> {
    let daif = crate::interrupts::save_and_disable_irq();
    for _ in 0..100_000 {
        if LOCK
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Some(Guard { daif });
        }
        core::hint::spin_loop();
    }
    crate::interrupts::restore_irq(daif);
    None
}

impl Drop for Guard {
    fn drop(&mut self) {
        LOCK.store(false, Ordering::Release);
        crate::interrupts::restore_irq(self.daif);
    }
}

/// Mirrors kernel console output into the ring. Called for every kernel
/// `write_str`, `write_bytes` and `write_byte`; EL0 output uses the unlogged
/// console path. Never writes to the console itself.
pub fn capture(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let bytes = if bytes.len() > CAPACITY {
        &bytes[bytes.len() - CAPACITY..]
    } else {
        bytes
    };
    let Some(_guard) = acquire() else {
        return;
    };
    let mut head = HEAD.load(Ordering::Relaxed);
    unsafe {
        let buffer = &mut *core::ptr::addr_of_mut!(CONSOLE_BUFFER);
        let first = bytes.len().min(CAPACITY - head);
        buffer[head..head + first].copy_from_slice(&bytes[..first]);
        let rest = bytes.len() - first;
        if rest != 0 {
            buffer[..rest].copy_from_slice(&bytes[first..]);
        }
    }
    head = (head + bytes.len()) % CAPACITY;
    HEAD.store(head, Ordering::Release);
    let total = TOTAL.load(Ordering::Relaxed);
    TOTAL.store(total.saturating_add(bytes.len()), Ordering::Release);
}

/// Fixed-capacity line builder; overflow truncates on a character boundary.
struct Buffer {
    bytes: [u8; LINE_CAPACITY],
    len: usize,
}

impl Buffer {
    fn new() -> Self {
        Self {
            bytes: [0; LINE_CAPACITY],
            len: 0,
        }
    }

    fn push(&mut self, text: &str) {
        let room = LINE_CAPACITY - self.len;
        let mut take = room.min(text.len());
        while !text.is_char_boundary(take) {
            take -= 1;
        }
        self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
    }

    fn finish(&mut self) {
        if self.len + 2 > LINE_CAPACITY {
            self.len = LINE_CAPACITY - 2;
        }
        self.bytes[self.len] = b'\r';
        self.bytes[self.len + 1] = b'\n';
        self.len += 2;
    }

    fn data(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl fmt::Write for Buffer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.push(text);
        Ok(())
    }
}

/// One formatted log record. Macros capture `file!()`/`line!()`, so callers
/// never spell the source location by hand.
pub fn event(
    level: Level,
    target: &str,
    file: &'static str,
    line: u32,
    why: Option<&str>,
    arguments: fmt::Arguments<'_>,
) {
    if !enabled(level) {
        return;
    }
    let total_ms = crate::boot_metrics::elapsed_millis();
    let seconds = total_ms / 1000;
    let millis = total_ms % 1000;
    let mut buffer = Buffer::new();
    let _ = write!(
        buffer,
        "[{:>5}.{:03}] {:<5} {:<10} {}:{}  ",
        seconds,
        millis,
        level.name(),
        target,
        file,
        line
    );
    let _ = buffer.write_fmt(arguments);
    if let Some(why) = why {
        let _ = write!(buffer, " (why: {})", why);
    }
    buffer.finish();
    crate::console::write_bytes(buffer.data());
}

/// Copies the console ring in chronological order. Returns bytes written
/// (0 when empty or busy). The stream may start mid-line: it is a raw tail.
pub fn snapshot(out: &mut [u8]) -> usize {
    let Some(_guard) = acquire() else {
        return 0;
    };
    let total = TOTAL.load(Ordering::Acquire);
    let length = total.min(CAPACITY);
    if length == 0 || out.is_empty() {
        return 0;
    }
    let head = HEAD.load(Ordering::Acquire);
    let take = length.min(out.len());
    let skip = length - take;
    let start = (head + CAPACITY - length + skip) % CAPACITY;
    let buffer = unsafe { &*core::ptr::addr_of!(CONSOLE_BUFFER) };
    let first = take.min(CAPACITY - start);
    out[..first].copy_from_slice(&buffer[start..start + first]);
    if take > first {
        out[first..take].copy_from_slice(&buffer[..take - first]);
    }
    take
}

/// Prints the newest `limit` bytes of the ring. The panic path calls this
/// before its banner, so the banner is not duplicated in the history.
pub fn dump_tail(limit: usize) {
    let mut buffer = [0_u8; 2048];
    let limit = limit.min(buffer.len());
    let written = {
        let Some(_guard) = acquire() else {
            return;
        };
        let total = TOTAL.load(Ordering::Acquire);
        let length = total.min(CAPACITY).min(limit);
        if length == 0 {
            return;
        }
        let head = HEAD.load(Ordering::Acquire);
        let start = (head + CAPACITY - length) % CAPACITY;
        let buffer_ref = unsafe { &*core::ptr::addr_of!(CONSOLE_BUFFER) };
        let first = length.min(CAPACITY - start);
        buffer[..first].copy_from_slice(&buffer_ref[start..start + first]);
        if length > first {
            buffer[first..length].copy_from_slice(&buffer_ref[..length - first]);
        }
        length
    };
    crate::console::write_bytes(&buffer[..written]);
}

#[macro_export]
macro_rules! log_error {
    ($target:expr, $($arg:tt)*) => {
        $crate::log::event($crate::log::Level::Error, $target, file!(), line!(), None, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($target:expr, $($arg:tt)*) => {
        $crate::log::event($crate::log::Level::Warn, $target, file!(), line!(), None, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($target:expr, $($arg:tt)*) => {
        $crate::log::event($crate::log::Level::Info, $target, file!(), line!(), None, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($target:expr, $($arg:tt)*) => {
        $crate::log::event($crate::log::Level::Debug, $target, file!(), line!(), None, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_trace {
    ($target:expr, $($arg:tt)*) => {
        $crate::log::event($crate::log::Level::Trace, $target, file!(), line!(), None, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error_why {
    ($target:expr, $why:expr, $($arg:tt)*) => {
        $crate::log::event($crate::log::Level::Error, $target, file!(), line!(), Some($why), format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn_why {
    ($target:expr, $why:expr, $($arg:tt)*) => {
        $crate::log::event($crate::log::Level::Warn, $target, file!(), line!(), Some($why), format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info_why {
    ($target:expr, $why:expr, $($arg:tt)*) => {
        $crate::log::event($crate::log::Level::Info, $target, file!(), line!(), Some($why), format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug_why {
    ($target:expr, $why:expr, $($arg:tt)*) => {
        $crate::log::event($crate::log::Level::Debug, $target, file!(), line!(), Some($why), format_args!($($arg)*))
    };
}
