//! Shared Content-Length JSON-RPC framing: the LSP base protocol and
//! the DAP transport are the same wire format (`Content-Length: N\r\n`
//! `\r\n` + N body bytes). Consumers: `lsp.rs`, `dap.rs`. MCP stdio is
//! newline-delimited and deliberately NOT this — its reader is the line
//! codec. No new dependency, same precedent as everything in this
//! crate's protocol layer.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A hostile or buggy Content-Length must not drive an allocation:
/// frames larger than this close the connection instead.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Write one framed body.
pub async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    w: &mut W,
    body: &[u8],
) -> std::io::Result<()> {
    w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    w.write_all(body).await?;
    w.flush().await
}

/// Read one framed body. `None` = the pipe is done (clean EOF, a framing
/// lie bigger than [`MAX_FRAME`], or a mid-frame read error — in every
/// one of those cases the only sane move is to stop reading).
/// `Some(bytes)` with an empty vec means a zero-length frame; callers
/// that require JSON will fail the parse and keep looping, mirroring
/// the "stray blank lines between messages" tolerance of the header
/// reader.
pub async fn read_frame<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Option<Vec<u8>> {
    let headers = read_headers(reader).await.ok()??;
    // header names are case-insensitive per the LSP base protocol
    let len: usize = headers
        .iter()
        .find_map(|h| {
            let (k, v) = h.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    match len {
        0 => Some(Vec::new()),
        l if l > MAX_FRAME => None,
        l => {
            let mut body = vec![0u8; l];
            reader.read_exact(&mut body).await.ok()?;
            Some(body)
        }
    }
}

/// Read one header block (to the blank line); `None` = EOF.
async fn read_headers<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<String>>> {
    let mut headers: Vec<String> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        // read one line terminated by \n (header lines end \r\n)
        let mut line = Vec::new();
        loop {
            match reader.read(&mut byte).await {
                Ok(0) | Err(_) => return Ok(None),
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => line.push(byte[0]),
            }
        }
        let line = String::from_utf8_lossy(&line)
            .trim_end_matches('\r')
            .to_string();
        if line.is_empty() {
            if headers.is_empty() {
                continue; // stray blank lines between messages
            }
            return Ok(Some(std::mem::take(&mut headers)));
        }
        headers.push(line);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Fixed-seed xorshift64*: deterministic pseudo-random inputs without
    /// pulling in a dependency. Only used by the property tests below.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed | 1) // xorshift is stuck at zero
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            assert!(n > 0, "below() needs a non-empty range");
            (self.next_u64() % n as u64) as usize
        }

        fn byte(&mut self) -> u8 {
            self.next_u64() as u8
        }
    }

    const SEEDS: [u64; 3] = [
        0x9E37_79B9_7F4A_7C15,
        0x1234_5678_9ABC_DEF1,
        0xDEAD_BEEF_CAFE_F00D,
    ];

    /// Bytes that stress the header/body boundary and CRLF handling.
    const BODY_ALPHABET: [u8; 9] = [b'\r', b'\n', b'"', b'\\', 0x00, 0xff, b'{', b'}', b' '];

    fn random_body(rng: &mut Rng, max_len: usize) -> Vec<u8> {
        let len = rng.below(max_len + 1);
        let mut body = Vec::with_capacity(len);
        for _ in 0..len {
            body.push(if rng.below(2) == 0 {
                BODY_ALPHABET[rng.below(BODY_ALPHABET.len())]
            } else {
                rng.byte()
            });
        }
        body
    }

    #[tokio::test]
    async fn frames_round_trip_and_tolerate_noise() {
        let mut buf = Vec::new();
        write_frame(&mut buf, br#"{"a":1}"#).await.unwrap();
        write_frame(&mut buf, b"second").await.unwrap();
        let mut r = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut r).await.unwrap(), br#"{"a":1}"#);
        assert_eq!(read_frame(&mut r).await.unwrap(), b"second");
        assert!(read_frame(&mut r).await.is_none(), "EOF is None");
    }

    #[tokio::test]
    async fn zero_length_frames_surface_as_empty() {
        // callers fail the JSON parse on an empty body and keep looping
        // — same net effect as the pre-extraction `continue`
        let mut buf = Vec::new();
        write_frame(&mut buf, b"").await.unwrap();
        write_frame(&mut buf, b"after").await.unwrap();
        let mut r = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut r).await.unwrap(), Vec::<u8>::new());
        assert_eq!(read_frame(&mut r).await.unwrap(), b"after");
    }

    #[tokio::test]
    async fn oversize_frames_close_instead_of_allocating() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"tiny").await.unwrap();
        let mut hostile = format!("Content-Length: {}\r\n\r\n", MAX_FRAME + 1).into_bytes();
        hostile.extend_from_slice(&buf);
        let mut r = std::io::Cursor::new(hostile);
        assert!(read_frame(&mut r).await.is_none(), "a framing lie closes");
    }

    #[tokio::test]
    async fn content_length_is_case_insensitive() {
        let mut buf = b"content-length: 2\r\n\r\nhi".to_vec();
        let mut r = std::io::Cursor::new(&mut buf);
        assert_eq!(read_frame(&mut r).await.unwrap(), b"hi");
    }

    #[tokio::test]
    async fn round_trip_random_bodies_preserves_exact_bytes() {
        for seed in SEEDS {
            let mut rng = Rng::new(seed);
            for _ in 0..200 {
                let body = random_body(&mut rng, 1024);
                let mut buf = Vec::new();
                write_frame(&mut buf, &body).await.unwrap();
                let mut r: &[u8] = &buf;
                assert_eq!(
                    read_frame(&mut r).await.as_deref(),
                    Some(body.as_slice()),
                    "seed {seed:#x}, body len {}",
                    body.len()
                );
            }
        }
    }

    #[tokio::test]
    async fn prefix_truncation_is_none_until_complete() {
        for seed in SEEDS {
            let mut rng = Rng::new(seed);
            // exhaustive: bodies up to 64 bytes
            for _ in 0..20 {
                let body = random_body(&mut rng, 64);
                let mut frame = Vec::new();
                write_frame(&mut frame, &body).await.unwrap();
                for n in 0..frame.len() {
                    let mut r: &[u8] = &frame[..n];
                    assert!(
                        read_frame(&mut r).await.is_none(),
                        "strict prefix {n}/{} must be None (seed {seed:#x})",
                        frame.len()
                    );
                }
                let mut r: &[u8] = &frame;
                assert_eq!(read_frame(&mut r).await.as_deref(), Some(body.as_slice()));
            }
            // sampled: larger bodies across the buffer we just built
            for _ in 0..20 {
                let body = random_body(&mut rng, 1024);
                let mut frame = Vec::new();
                write_frame(&mut frame, &body).await.unwrap();
                let header_end = frame
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|p| p + 4)
                    .expect("write_frame always emits the blank line");
                let mut samples = vec![
                    0,
                    header_end.saturating_sub(1),
                    header_end,
                    header_end + body.len() / 2,
                    frame.len() - 1,
                ];
                for _ in 0..40 {
                    samples.push(rng.below(frame.len()));
                }
                for n in samples {
                    if n >= frame.len() {
                        continue; // the complete frame was asserted above
                    }
                    let mut r: &[u8] = &frame[..n];
                    assert!(
                        read_frame(&mut r).await.is_none(),
                        "strict prefix {n}/{} must be None (seed {seed:#x})",
                        frame.len()
                    );
                }
                let mut r: &[u8] = &frame;
                assert_eq!(read_frame(&mut r).await.as_deref(), Some(body.as_slice()));
            }
        }
    }

    #[tokio::test]
    async fn random_junk_streams_never_panic() {
        for seed in SEEDS {
            let mut rng = Rng::new(seed);
            for _ in 0..200 {
                let len = rng.below(513);
                let mut junk = Vec::with_capacity(len);
                for _ in 0..len {
                    junk.push(rng.byte());
                }
                // no panic is the property; whatever comes back must fit
                // the same contract the read_frame doc states
                let mut r: &[u8] = &junk;
                let _ = read_frame(&mut r).await;
            }
        }
    }

    #[tokio::test]
    async fn header_fuzz_is_safe_and_exact_lies_close() {
        for seed in SEEDS {
            let mut rng = Rng::new(seed);
            for _ in 0..200 {
                let body = random_body(&mut rng, 32);
                let mut raw = Vec::new();
                // unrelated / malformed headers before the real one
                for _ in 0..rng.below(4) {
                    match rng.below(3) {
                        0 => raw.extend_from_slice(b"X-Noise: 1\r\n"),
                        1 => raw.extend_from_slice(b"no-colon-here\r\n"),
                        _ => raw.extend_from_slice(b"Content-Length : 7\r\n"),
                    }
                }
                // random case variant of the header name
                let name: String = "Content-Length"
                    .bytes()
                    .map(|b| {
                        (if rng.below(2) == 0 {
                            b.to_ascii_uppercase()
                        } else {
                            b.to_ascii_lowercase()
                        }) as char
                    })
                    .collect();
                let spaces = " ".repeat(rng.below(3));
                raw.extend_from_slice(format!("{name}:{spaces}{}\r\n\r\n", body.len()).as_bytes());
                raw.extend_from_slice(&body);
                let mut r: &[u8] = &raw;
                assert_eq!(
                    read_frame(&mut r).await.as_deref(),
                    Some(body.as_slice()),
                    "seed {seed:#x}, name {name:?}, body len {}",
                    body.len()
                );
            }
        }

        // numeric string that overflows usize: parse fails, no panic
        let huge = b"Content-Length: 99999999999999999999999999\r\n\r\n".to_vec();
        let mut r: &[u8] = &huge;
        let _ = read_frame(&mut r).await;

        // declared length > MAX_FRAME with no body bytes: None, promptly
        for hostile_len in [MAX_FRAME + 1, usize::MAX] {
            let hostile = format!("Content-Length: {hostile_len}\r\n\r\n").into_bytes();
            let mut r: &[u8] = &hostile;
            assert!(
                read_frame(&mut r).await.is_none(),
                "length {hostile_len} must close, not allocate"
            );
        }

        // declared zero is an empty frame, not EOF
        let zero = b"Content-Length: 0\r\n\r\n".to_vec();
        let mut r: &[u8] = &zero;
        assert_eq!(read_frame(&mut r).await, Some(Vec::new()));
    }

    #[tokio::test]
    async fn leading_blank_lines_are_tolerated() {
        let mut r: &[u8] = b"\r\n\r\nContent-Length: 2\r\n\r\nab";
        assert_eq!(read_frame(&mut r).await.as_deref(), Some(&b"ab"[..]));
    }
}
