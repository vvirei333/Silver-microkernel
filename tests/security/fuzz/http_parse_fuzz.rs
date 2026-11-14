// Host-side fuzz harness for the HTTP/1.x response parser.
//
// Compiled with rustc (std is fine here); the parser itself is the same
// core-only module the guest network server uses (`userspace/src/http_parse.rs`).
// A panic is a finding: the parser is fed bytes from an arbitrary peer, so it
// must never index out of bounds or overflow, no matter how `received` and the
// buffer disagree.
//
//   rustc --edition 2021 -O tests/security/fuzz/http_parse_fuzz.rs -o /tmp/http_fuzz
//   /tmp/http_fuzz            # or: tools/security_test.py --fuzz
#[path = "../../../userspace/src/http_parse.rs"]
mod http_parse;

use std::panic::{catch_unwind, AssertUnwindSafe};

const SEEDS: &[&[u8]] = &[
    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
    b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n",
    b"HTTP/1.1 204 No Content\r\n\r\n",
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: CHUNKED\r\n\r\n1A\r\n0123456789abcdefghijklmnopqrstuv\r\n0\r\n\r\n",
    b"HTTP/1.1 301 Moved\r\nLocation: /next\r\nContent-Length: 0\r\n\r\n",
    b"HTTP/1.1 200 OK\r\nX-Empty:\r\nX-Space:   value  \r\n\r\nbody",
    b"HTTP/1.1 200 OK\r\nContent-Length: 999999999999999999999\r\n\r\nx",
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFFFFFFFFFFFFFFFFFF\r\nx",
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5",
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\r\n",
    b"HTTP/1.0 200",
    b"\r\n\r\n",
    b"",
];

fn xorshift(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    value
}

fn mutate(buffer: &mut Vec<u8>, state: &mut u64) {
    let operations = (xorshift(state) % 8) + 1;
    for _ in 0..operations {
        if buffer.is_empty() {
            buffer.push(b'\r');
            continue;
        }
        let index = (xorshift(state) as usize) % buffer.len();
        match xorshift(state) % 6 {
            0 => buffer[index] = (xorshift(state) & 0xff) as u8,
            1 => buffer[index] = b'\r',
            2 => buffer[index] = b'\n',
            3 => buffer[index] = b':',
            4 => {
                buffer.insert(index, (xorshift(state) & 0xff) as u8);
                if buffer.len() > 8192 {
                    buffer.pop();
                }
            }
            _ => {
                buffer.remove(index);
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn main() {
    // Named regression first: a chunk size of 0xFFF...F used to make the
    // CRLF skip wrap `input + 1` to zero and spin forever (DoS). If this
    // hangs again, the harness never reaches the random phase.
    let mut spin = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                     FFFFFFFFFFFFFFFFFFFFFFFF\r\nx"
        .to_vec();
    let spin_len = spin.len();
    http_parse::parse_response(&mut spin, spin_len);

    let mut state = 0x243F_6A88_85A3_08D3u64;
    let iterations: u64 = std::env::var("HTTP_FUZZ_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200_000);
    let mut crashes = 0u64;

    for iteration in 0..iterations {
        let seed = SEEDS[(xorshift(&mut state) as usize) % SEEDS.len()];
        let mut buffer = seed.to_vec();
        if xorshift(&mut state) & 1 == 0 {
            mutate(&mut buffer, &mut state);
        }
        let received = match xorshift(&mut state) % 5 {
            0 => buffer.len(),
            1 => buffer.len().saturating_add(1 + (xorshift(&mut state) as usize % 64)),
            2 => 0,
            3 => (xorshift(&mut state) as usize) % (buffer.len() + 1),
            _ => buffer.len().saturating_sub(1),
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            http_parse::parse_response(&mut buffer, received);
        }));
        if outcome.is_err() {
            crashes += 1;
            eprintln!(
                "PANIC iteration={iteration} len={} received={received} input={}",
                buffer.len(),
                hex(&buffer)
            );
            if crashes >= 5 {
                break;
            }
        }
    }

    if crashes != 0 {
        eprintln!("http parser fuzz: {crashes} panic(s) in {iterations} cases");
        std::process::exit(1);
    }
    println!("http parser fuzz ok: {iterations} cases");
}
