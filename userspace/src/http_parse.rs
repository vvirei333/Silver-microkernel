//! HTTP/1.x response parsing.
//!
//! Core-only (no `std`, no allocation) so the same code runs in the guest's
//! network server and in the host-side fuzz harness under
//! `tests/security/fuzz/`. Everything here treats the input as hostile: the
//! server receives it from an arbitrary peer, and the fuzzer feeds it random
//! and mutated bytes.
//!
//! Wire format handled: status line, headers, `Content-Length` and
//! `Transfer-Encoding: chunked`. The body is decoded in place; the returned
//! length is the number of bytes starting at the body offset.

/// Finds the end of the header block (`\r\n\r\n`), returning the index of the
/// first `\r`.
pub fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Finds a header value (case-insensitive) after the status line.
pub fn find_header<'a>(headers: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    for line in headers.split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.len() > name.len()
            && line[..name.len()].eq_ignore_ascii_case(name)
            && line[name.len()] == b':'
        {
            let value = &line[name.len() + 1..];
            let start = value.iter().position(|byte| !byte.is_ascii_whitespace())?;
            return Some(&value[start..]);
        }
    }
    None
}

/// Parses a response. Returns `(status, body_len)` or `None` when the header
/// block is incomplete/malformed. `received` is clamped to the buffer: a
/// caller that over-reports the byte count must not make the parser read past
/// the buffer.
pub fn parse_response(buffer: &mut [u8], received: usize) -> Option<(u16, usize)> {
    let received = received.min(buffer.len());
    let end = find_header_end(&buffer[..received])?;
    let headers = &buffer[..end];
    let body_start = end + 4;

    // "HTTP/1.1 200 OK"
    let status = headers
        .split(|&byte| byte == b' ')
        .nth(1)
        .and_then(|digits| core::str::from_utf8(digits).ok())
        .and_then(|digits| digits.trim().parse::<u16>().ok())
        .unwrap_or(0);

    if let Some(value) = find_header(headers, b"transfer-encoding") {
        if value.eq_ignore_ascii_case(b"chunked") {
            let length = dechunk(&mut buffer[body_start..received]);
            return Some((status, length));
        }
    }
    if let Some(value) = find_header(headers, b"content-length") {
        if let Ok(text) = core::str::from_utf8(value) {
            if let Ok(length) = text.trim().parse::<usize>() {
                let available = received - body_start;
                return Some((status, length.min(available)));
            }
        }
    }
    Some((status, received - body_start))
}

/// Decodes a chunked body in place. Returns the decoded length.
pub fn dechunk(buffer: &mut [u8]) -> usize {
    let mut input = 0usize;
    let mut output = 0usize;
    while input < buffer.len() {
        // Parse the hex chunk size.
        let mut size = 0usize;
        let mut digits = 0usize;
        while input < buffer.len() {
            let byte = buffer[input];
            let digit = match byte {
                b'0'..=b'9' => Some(usize::from(byte - b'0')),
                b'a'..=b'f' => Some(usize::from(byte - b'a') + 10),
                b'A'..=b'F' => Some(usize::from(byte - b'A') + 10),
                _ => None,
            };
            let Some(digit) = digit else { break };
            size = size.saturating_mul(16).saturating_add(digit);
            digits += 1;
            input += 1;
        }
        if digits == 0 {
            break;
        }
        // Skip the CRLF after the size.
        while input < buffer.len() && buffer[input] != b'\n' {
            input += 1;
        }
        input = (input + 1).min(buffer.len());
        if size == 0 {
            break;
        }
        let count = size.min(buffer.len() - input).min(buffer.len() - output);
        buffer.copy_within(input..input + count, output);
        // A hostile chunk size can be `usize::MAX`; clamp here so the CRLF
        // skip below (`input + 1`) cannot wrap back to zero and spin forever.
        input = input.saturating_add(size).min(buffer.len());
        output += count;
        // Skip the CRLF after the chunk data.
        while input < buffer.len() && buffer[input] != b'\n' {
            input += 1;
        }
        input = (input + 1).min(buffer.len());
    }
    output
}
