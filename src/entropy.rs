//! Kernel entropy pool and CSPRNG.
//!
//! There is no RTC and no dedicated virtio-rng device, so the pool mixes every
//! source the kernel actually has:
//!
//! * `CNTVCT_EL0` jitter sampled around variable work. An EL0 task can read the
//!   same counter through the architected timer, but not the *deltas* the
//!   kernel observed while other work was in flight; those carry the entropy.
//! * `RNDR` when `ID_AA64ISAR0_EL1` advertises the Armv8.5 RNG extension
//!   (QEMU's default `cortex-a53` does not, real hardware usually does).
//! * boot-time addresses, the tick counter and the current task id.
//!
//! The output is ChaCha20 in counter mode with a 256-bit key. The state is
//! re-keyed with fresh samples every [`RESEED_BYTES`], so a long-lived task
//! does not consume one fixed stream forever.
//!
//! This replaces the old `timer::ticks() ^ constant` xorshift: ticks are
//! published to EL0 by `clock_gettime`, so an observer could reconstruct the
//! stream and predict `getrandom`, `AT_RANDOM` and TLS handshake randomness.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::mutex::Mutex;

/// Bytes emitted before the key is mixed with fresh samples again.
const RESEED_BYTES: usize = 1024 * 1024;

struct Drbg {
    key: [u32; 8],
    counter: u64,
    block: [u8; 64],
    index: usize,
    seeded: bool,
    emitted: usize,
}

impl Drbg {
    const fn new() -> Self {
        Self {
            key: [0; 8],
            counter: 0,
            block: [0; 64],
            index: 64,
            seeded: false,
            emitted: 0,
        }
    }

    fn reseed(&mut self) {
        let mut seed = [0_u8; 32];
        fill_seed(&mut seed);
        for (word, chunk) in self.key.iter_mut().zip(seed.chunks_exact(4)) {
            *word ^= u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        self.counter = 0;
        self.index = 64;
        self.emitted = 0;
        self.seeded = true;
    }

    fn block(&mut self) {
        let state = [
            0x6170_7865,
            0x3320_646e,
            0x7962_2d32,
            0x6b20_6574,
            self.key[0],
            self.key[1],
            self.key[2],
            self.key[3],
            self.key[4],
            self.key[5],
            self.key[6],
            self.key[7],
            self.counter as u32,
            (self.counter >> 32) as u32,
            self.key[0] ^ self.key[5],
            self.key[2] ^ self.key[7],
        ];
        let mut working = state;
        for _ in 0..10 {
            quarter_round(&mut working, 0, 4, 8, 12);
            quarter_round(&mut working, 1, 5, 9, 13);
            quarter_round(&mut working, 2, 6, 10, 14);
            quarter_round(&mut working, 3, 7, 11, 15);
            quarter_round(&mut working, 0, 5, 10, 15);
            quarter_round(&mut working, 1, 6, 11, 12);
            quarter_round(&mut working, 2, 7, 8, 13);
            quarter_round(&mut working, 3, 4, 9, 14);
        }
        for (word, original) in working.iter_mut().zip(state.iter()) {
            *word = word.wrapping_add(*original);
        }
        for (chunk, word) in self.block.chunks_exact_mut(4).zip(working.iter()) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        self.counter = self.counter.wrapping_add(1);
        self.index = 0;
    }

    fn fill(&mut self, out: &mut [u8]) {
        if !self.seeded || self.emitted >= RESEED_BYTES {
            self.reseed();
        }
        for byte in out.iter_mut() {
            if self.index >= self.block.len() {
                self.block();
            }
            *byte = self.block[self.index];
            self.index += 1;
        }
        self.emitted += out.len();
    }
}

static DRBG: Mutex<Drbg> = Mutex::new(Drbg::new());
static HW_RNG: AtomicBool = AtomicBool::new(false);
static HW_RNG_KNOWN: AtomicBool = AtomicBool::new(false);

fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

/// Fills `out` from the kernel CSPRNG. Safe to call from any kernel context
/// (the pool is behind the kernel's spin mutex), including on the syscall path
/// and while building an initial Linux stack.
pub fn fill(out: &mut [u8]) {
    DRBG.lock().fill(out);
}

pub fn u64() -> u64 {
    let mut bytes = [0_u8; 8];
    fill(&mut bytes);
    u64::from_le_bytes(bytes)
}

fn cycle_counter() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntvct_el0", out(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Reads `ID_AA64ISAR0_EL1.RNDR` (bits 63:60) once.
fn hardware_rng() -> bool {
    if !HW_RNG_KNOWN.load(Ordering::Relaxed) {
        let isar: u64;
        unsafe {
            core::arch::asm!("mrs {}, id_aa64isar0_el1", out(reg) isar, options(nostack, preserves_flags));
        }
        HW_RNG.store((isar >> 60) & 0xf != 0, Ordering::Relaxed);
        HW_RNG_KNOWN.store(true, Ordering::Relaxed);
    }
    HW_RNG.load(Ordering::Relaxed)
}

/// One `RNDR` sample, or `None` when the instruction reports failure.
fn rndr() -> Option<u64> {
    if !hardware_rng() {
        return None;
    }
    let value: u64;
    let flags: u64;
    unsafe {
        core::arch::asm!(
            "mrs {value}, s3_3_c2_c4_0",
            "mrs {flags}, nzcv",
            value = out(reg) value,
            flags = out(reg) flags,
            options(nostack),
        );
    }
    // RNDR sets PSTATE.Z on failure.
    if flags & (1 << 30) != 0 {
        None
    } else {
        Some(value)
    }
}

fn mix(value: u64) -> u64 {
    // splitmix64 finalizer: cheap avalanche for timing deltas.
    let mut z = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn fill_seed(out: &mut [u8; 32]) {
    let mut accumulator = [0_u64; 4];
    for round in 0..64_u64 {
        let before = cycle_counter();
        // Variable work makes the two counter reads sample different pipeline
        // and cache states; the delta is what an EL0 observer cannot predict.
        let mut work = round | 1;
        for _ in 0..(round % 13) {
            work = work.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        }
        core::hint::black_box(work);
        let after = cycle_counter();
        accumulator[(round & 3) as usize] ^= mix(
            after
                .wrapping_sub(before)
                .rotate_left((round & 63) as u32)
                ^ round,
        );
    }
    for slot in accumulator.iter_mut() {
        if let Some(sample) = rndr() {
            *slot ^= mix(sample);
        }
    }
    let marker = 0_u8;
    accumulator[0] ^= (&marker as *const u8) as u64;
    accumulator[1] ^= crate::timer::ticks();
    accumulator[2] ^= crate::scheduler::current_task_id();
    accumulator[3] ^= mix(cycle_counter());

    for (index, chunk) in out.chunks_mut(8).enumerate() {
        let value = mix(accumulator[index % accumulator.len()] ^ (index as u64));
        chunk.copy_from_slice(&value.to_le_bytes()[..chunk.len()]);
    }
}

#[cfg(test)]
mod tests {
    // The pool is aarch64-only; nothing to exercise on the host test target.
}
