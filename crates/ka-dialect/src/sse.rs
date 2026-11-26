//! Minimal hand-rolled SSE parser. Handles `data:` lines, ignores `event:`
//! lines (both wires carry full payloads in data), comments, and CRLF.
//!
//! Buffers raw bytes and decodes UTF-8 only at complete-frame
//! boundaries — a multi-byte character split across stream chunks must
//! survive intact (per-chunk lossy decoding would emit U+FFFD twice
//! and silently corrupt CJK/emoji/accented output).

/// One parsed SSE frame (its `data:` payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The data payload (possibly multi-line, joined with `\n`).
    pub data: String,
}

/// Maximum bytes buffered for a single incomplete SSE frame before it is
/// dropped to protect against unbounded memory growth on hostile/corrupt
/// streams.
pub const MAX_SSE_BUF_BYTES: usize = 16 * 1024 * 1024;

/// Incremental SSE parser: feed raw bytes, receive complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    /// Offset in `buf` from which the next separator scan resumes, avoiding
    /// O(N^2) re-scanning on large chunked frames.
    scan_pos: usize,
    /// True while discarding the tail of an oversized frame until its
    /// terminating blank-line separator arrives.
    dropping_oversized: bool,
}

impl SseParser {
    /// New empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns any complete events decoded from them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        let mut remaining = bytes;
        while !remaining.is_empty() {
            if self.dropping_oversized {
                let avail = MAX_SSE_BUF_BYTES.saturating_sub(self.buf.len()).max(1);
                let take = remaining.len().min(avail);
                self.buf.extend_from_slice(&remaining[..take]);
                remaining = &remaining[take..];
                if let Some((idx, sep_len)) = find_separator(&self.buf, self.scan_pos) {
                    self.buf.drain(..idx + sep_len);
                    self.scan_pos = 0;
                    self.dropping_oversized = false;
                    self.drain_into(&mut out);
                } else {
                    // Retain only the last 3 bytes so a separator split across
                    // chunks (up to 4 bytes for \r\n\r\n) can still be matched.
                    let drop_len = self.buf.len().saturating_sub(3);
                    self.buf.drain(..drop_len);
                    self.scan_pos = 0;
                }
                continue;
            }

            let avail = MAX_SSE_BUF_BYTES.saturating_sub(self.buf.len());
            if avail == 0 {
                // Buffer is already at the cap without a separator: drop this
                // oversized frame and discard until its separator.
                let drop_len = self.buf.len().saturating_sub(3);
                self.buf.drain(..drop_len);
                self.scan_pos = 0;
                self.dropping_oversized = true;
                continue;
            }

            let take = remaining.len().min(avail);
            self.buf.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];

            // strip a leading UTF-8 BOM at the buffer head (checked every
            // feed: a BOM split across a tiny first chunk would otherwise
            // survive the once-at-start strip and break the first frame).
            // Safe to re-check: `buf`'s head is only ever at a frame
            // boundary, and a frame starts with an ASCII field name —
            // never with 0xEF, a UTF-8 lead byte.
            if self.buf.starts_with([0xEF, 0xBB, 0xBF].as_slice()) {
                self.buf.drain(..3);
                self.scan_pos = self.scan_pos.saturating_sub(3);
            }

            self.drain_into(&mut out);

            if self.buf.len() >= MAX_SSE_BUF_BYTES {
                let drop_len = self.buf.len().saturating_sub(3);
                self.buf.drain(..drop_len);
                self.scan_pos = 0;
                self.dropping_oversized = true;
            }
        }
        out
    }

    /// Flush at end-of-stream: a trailing frame without final separator is
    /// still an event if it contains a data line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        if self.dropping_oversized {
            self.buf.clear();
            self.scan_pos = 0;
            self.dropping_oversized = false;
            return Vec::new();
        }
        let mut out = Vec::new();
        self.drain_into(&mut out);
        if let Some(evt) = parse_frame(&String::from_utf8_lossy(&self.buf)) {
            out.push(evt);
        }
        self.buf.clear();
        self.scan_pos = 0;
        out
    }

    fn drain_into(&mut self, out: &mut Vec<SseEvent>) {
        // Split on blank-line separators: "\n\n", "\r\n\r\n", or mixed.
        while let Some((idx, sep_len)) = find_separator(&self.buf, self.scan_pos) {
            let frame: Vec<u8> = self.buf.drain(..idx + sep_len).collect();
            self.scan_pos = 0;
            // decode the frame WITHOUT the separator, at a char boundary
            let frame = String::from_utf8_lossy(&frame[..idx]);
            if let Some(evt) = parse_frame(&frame) {
                out.push(evt);
            }
        }
        // Keep up to 3 trailing bytes in the scan window so a 2..4 byte
        // separator split across chunks is always found on the next feed.
        self.scan_pos = self.buf.len().saturating_sub(3);
    }
}

/// Earliest blank-line separator at or after `start`: `(index, length)` for
/// `\n\n` (2), `\r\n\r\n` (4), or the spec-legal mixed `\n\r\n` (3).
fn find_separator(buf: &[u8], start: usize) -> Option<(usize, usize)> {
    if buf.len() < 2 || start >= buf.len() - 1 {
        return None;
    }
    for i in start..buf.len() - 1 {
        if buf[i] == b'\n' {
            if buf[i + 1] == b'\n' {
                return Some((i, 2));
            }
            // mixed line endings: LF then CRLF
            if buf.len() >= i + 3 && buf[i + 1] == b'\r' && buf[i + 2] == b'\n' {
                return Some((i, 3));
            }
        }
        if buf[i] == b'\r'
            && buf.len() >= i + 4
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some((i, 4));
        }
    }
    None
}

fn parse_frame(frame: &str) -> Option<SseEvent> {
    let mut data: Vec<&str> = Vec::new();
    for line in frame.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.strip_prefix(' ').unwrap_or(payload);
            data.push(payload);
        }
        // `event:`, `id:`, `retry:`, and `:comments` are ignored — both
        // wires carry the full payload in data lines
    }
    (!data.is_empty()).then(|| SseEvent {
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn splits_events_on_blank_lines() {
        let mut p = SseParser::new();
        let evts = p.feed(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        assert_eq!(evts.len(), 2);
        assert_eq!(evts[0].data, "{\"a\":1}");
        assert_eq!(evts[1].data, "{\"b\":2}");
    }

    #[test]
    fn handles_crlf_and_comments() {
        let mut p = SseParser::new();
        let evts = p.feed(b": keepalive\r\ndata: x\r\n\r\n");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, "x");
    }

    #[test]
    fn multi_byte_utf8_split_across_chunks_survives() {
        // "café" where é is 2 bytes — split the codepoint in half
        let mut p = SseParser::new();
        let a = p.feed(b"data: caf");
        assert!(a.is_empty());
        let b = p.feed(&[0xC3]); // first byte of é
        assert!(b.is_empty());
        let c = p.feed(&[0xA9, b'\n', b'\n']); // second byte + separator
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].data, "café", "no U+FFFD replacement chars");
        // CJK split mid-codepoint
        let mut p = SseParser::new();
        let text = "漢字テスト";
        let bytes = format!("data: {text}\n\n").into_bytes();
        let cut = bytes.len() - 3; // inside the last character
        assert!(p.feed(&bytes[..cut]).is_empty());
        let evts = p.feed(&bytes[cut..]);
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, text);
    }

    #[test]
    fn finish_flushes_trailing_frame() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: tail").is_empty());
        let evts = p.finish();
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, "tail");
        assert!(p.finish().is_empty(), "double finish is a no-op");
    }

    #[test]
    fn multi_line_data_joins_with_newlines() {
        let mut p = SseParser::new();
        let evts = p.feed(b"data: a\ndata: b\n\n");
        assert_eq!(evts[0].data, "a\nb");
    }

    #[test]
    fn mixed_lf_crlf_separator_splits_events() {
        let mut p = SseParser::new();
        let evts = p.feed(b"data: x\n\r\ndata: y\n\n");
        assert_eq!(evts.len(), 2);
        assert_eq!(evts[0].data, "x");
        assert_eq!(evts[1].data, "y");
    }

    #[test]
    fn leading_bom_is_stripped_once() {
        let mut p = SseParser::new();
        let bytes: Vec<u8> = [0xEF, 0xBB, 0xBF]
            .into_iter()
            .chain(b"data: bom\n\n".iter().copied())
            .collect();
        let evts = p.feed(&bytes);
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, "bom");
    }

    #[test]
    fn chunk_boundary_invariance() {
        let mut stream = vec![0xEF, 0xBB, 0xBF];
        stream.extend_from_slice(b": keepalive\r\n");
        stream.extend_from_slice("data: caf\u{00e9}\n\n".as_bytes());
        stream.extend_from_slice(b"data: line1\r\ndata: line2\r\n\r\n");
        stream.extend_from_slice("data: \u{6f22}\u{5b57}\u{30c6}\u{30b9}\u{30c8}\n\r\n".as_bytes());
        stream.extend_from_slice(b"event: delta\ndata: {\"k\":1}\n\n");
        stream.extend_from_slice(b"data: trailing-no-sep");

        let mut whole = SseParser::new();
        let mut expected = whole.feed(&stream);
        expected.extend(whole.finish());
        assert_eq!(expected.len(), 5);

        for chunk_size in 1..=17 {
            let mut p = SseParser::new();
            let mut got = Vec::new();
            for chunk in stream.chunks(chunk_size) {
                got.extend(p.feed(chunk));
            }
            got.extend(p.finish());
            assert_eq!(got, expected, "mismatch for chunk_size={chunk_size}");
        }
    }

    #[test]
    fn buffer_cap_drops_oversized_frame_and_recovers() {
        let mut p = SseParser::new();
        let block = vec![b'a'; 1024 * 1024];
        // Feed 17 MiB without any separator: buffer must stay bounded at MAX_SSE_BUF_BYTES.
        assert!(p.feed(b"data: ").is_empty());
        for _ in 0..17 {
            let evts = p.feed(&block);
            assert!(evts.is_empty());
            assert!(p.buf.len() <= MAX_SSE_BUF_BYTES);
        }
        // End the oversized frame with a separator, followed by a normal frame.
        let evts = p.feed(b"\r\n\r\ndata: recovered\n\n");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, "recovered");
        assert!(p.finish().is_empty());
    }
}
