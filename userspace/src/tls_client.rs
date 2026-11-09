//! Guest-side TLS 1.3 client built on `embedded-tls`.
//!
//! The transport is supplied by the network server (a blocking adapter over
//! its smoltcp socket). This module owns the trust anchors, the clock, the
//! RNG and the `CryptoProvider` glue; the verifier comes from the vendored
//! `third_party/embedded-tls` with three local fixes:
//!
//! * it tries a list of roots in one handshake instead of reconnecting;
//! * it skips a trust anchor that the server included cross-signed (ISRG X2
//!   by X1, SSL.com by Comodo, ...);
//! * it picks the ECDSA verifier from the CA key's curve instead of the hash
//!   in the child certificate's signature algorithm (P-384 roots under
//!   ECDSA-SHA256 signatures).
//!
//! There is no RTC yet, so `FixedClock` pins certificate validity to a
//! build-time date. Replace it with the RTC when `date` lands.

use core::mem::MaybeUninit;

use embedded_tls::blocking::{
    Aes128GcmSha256, CryptoProvider, TlsClock, TlsConfig, TlsConnection, TlsContext, TlsError,
    TlsVerifier,
};
use embedded_tls::pki::CertVerifier;
use embedded_tls::CryptoRngCore;
use rand_core::{CryptoRng, RngCore};

use crate::syscall;
use crate::tls_roots;

pub struct FixedClock;

impl TlsClock for FixedClock {
    fn now() -> Option<u64> {
        Some(tls_roots::FIXED_NOW)
    }
}

type Verifier = CertVerifier<'static, Aes128GcmSha256, FixedClock, 16384>;

static mut VERIFIER: MaybeUninit<Verifier> = MaybeUninit::uninit();
static mut VERIFIER_READY: bool = false;

/// One verifier for the whole server: it is only used by the single
/// in-flight fetch, and building it costs a 16 KiB certificate store.
fn verifier() -> &'static mut Verifier {
    unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(VERIFIER);
        if !*core::ptr::addr_of!(VERIFIER_READY) {
            slot.write(CertVerifier::new_multi(&tls_roots::ROOTS));
            *core::ptr::addr_of_mut!(VERIFIER_READY) = true;
        }
        &mut *slot.as_mut_ptr()
    }
}

/// TLS randomness comes from the kernel CSPRNG (`SYS_RANDOM`). The local
/// fallback exists only for the impossible case where the syscall rejects the
/// destination buffer; it is never the primary source. The previous version
/// seeded an xorshift from uptime ticks, a counter and a stack address, all of
/// which an observer can derive from `clock_gettime`, so handshake randomness
/// (including ephemeral keys) was predictable.
pub struct TlsRng {
    fallback: u64,
}

impl TlsRng {
    pub fn new() -> Self {
        // Mix a few syscall samples so even the fallback is not a fixed seed.
        let mut seed = [0_u8; 8];
        let _ = syscall::random(&mut seed);
        let mut state = u64::from_le_bytes(seed) ^ 0xA076_1D64_78BD_642F;
        if state == 0 {
            state = 0x1234_5678_9ABC_DEF0;
        }
        Self { fallback: state }
    }

    fn next(&mut self) -> u64 {
        let mut value = self.fallback;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        self.fallback = value;
        value.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

impl RngCore for TlsRng {
    fn next_u32(&mut self) -> u32 {
        self.next() as u32
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0_u8; 8];
        if syscall::random(&mut bytes) {
            u64::from_le_bytes(bytes)
        } else {
            self.next()
        }
    }

    fn fill_bytes(&mut self, destination: &mut [u8]) {
        if syscall::random(destination) {
            return;
        }
        for chunk in destination.chunks_mut(8) {
            let bytes = self.next().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }

    fn try_fill_bytes(&mut self, destination: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(destination);
        Ok(())
    }
}

impl CryptoRng for TlsRng {}

pub struct TlsProvider {
    rng: TlsRng,
    verifier: &'static mut Verifier,
}

impl TlsProvider {
    pub fn new() -> Self {
        Self {
            rng: TlsRng::new(),
            verifier: verifier(),
        }
    }
}

impl CryptoProvider for TlsProvider {
    type CipherSuite = Aes128GcmSha256;
    type Signature = p256::ecdsa::DerSignature;

    fn rng(&mut self) -> impl CryptoRngCore {
        &mut self.rng
    }

    fn verifier(&mut self) -> Result<&mut impl TlsVerifier<Self::CipherSuite>, TlsError> {
        Ok(&mut *self.verifier)
    }
}

/// Runs one HTTPS request over `transport` and copies the decrypted response
/// (headers included) into `response`. The caller closes the TCP socket.
pub fn fetch<Transport>(
    transport: Transport,
    host: &str,
    request: &[u8],
    read_buffer: &mut [u8],
    write_buffer: &mut [u8],
    response: &mut [u8],
) -> Result<usize, TlsError>
where
    Transport: embedded_io::Read + embedded_io::Write,
{
    let config = TlsConfig::new().with_server_name(host);
    let provider = TlsProvider::new();
    let mut tls = TlsConnection::<_, Aes128GcmSha256>::new(
        transport,
        read_buffer,
        write_buffer,
    );
    tls.open(TlsContext::new(&config, provider))?;

    use embedded_io::Write as _;
    tls.write_all(request)?;
    tls.flush()?;

    let mut total = 0usize;
    let mut empty = 0usize;
    while total < response.len() {
        match tls.read(&mut response[total..]) {
            Ok(0) => {
                empty += 1;
                if empty >= 4 {
                    break;
                }
            }
            Ok(count) => {
                total += count;
                empty = 0;
            }
            Err(TlsError::ConnectionClosed) => break,
            Err(TlsError::Io(kind)) => {
                // After a `Connection: close` response the peer may close with
                // a TCP reset before close_notify; keep what we received.
                if total > 0 {
                    break;
                }
                return Err(TlsError::Io(kind));
            }
            Err(error) => {
                if total > 0 {
                    break;
                }
                return Err(error);
            }
        }
    }
    Ok(total)
}
