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

/// Incremental SSE parser: feed raw bytes, receive complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    /// New empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns any complete events decoded from them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        // strip a leading UTF-8 BOM at the buffer head (checked every
        // feed: a BOM split across a tiny first chunk would otherwise
        // survive the once-at-start strip and break the first frame).
        // Safe to re-check: `buf`'s head is only ever at a frame
        // boundary, and a frame starts with an ASCII field name —
        // never with 0xEF, a UTF-8 lead byte.
        self.buf.extend_from_slice(bytes);
        if self.buf.starts_with([0xEF, 0xBB, 0xBF].as_slice()) {
            self.buf.drain(..3);
        }
        self.drain()
    }

    /// Flush at end-of-stream: a trailing frame without final separator is
    /// still an event if it contains a data line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = self.drain();
        if let Some(evt) = parse_frame(&String::from_utf8_lossy(&self.buf)) {
            out.push(evt);
        }
        self.buf.clear();
        out
    }

    fn drain(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        // Split on blank-line separators: "\n\n", "\r\n\r\n", or mixed.
        while let Some((idx, sep_len)) = find_separator(&self.buf) {
            let frame: Vec<u8> = self.buf.drain(..idx + sep_len).collect();
            // decode the frame WITHOUT the separator, at a char boundary
            let frame = String::from_utf8_lossy(&frame[..idx]);
            if let Some(evt) = parse_frame(&frame) {
                out.push(evt);
            }
        }
        out
    }
}

/// Earliest blank-line separator: `(index, length)` for `\n\n` (2),
/// `\r\n\r\n` (4), or the spec-legal mixed `\n\r\n` (3).
fn find_separator(buf: &[u8]) -> Option<(usize, usize)> {
    if buf.len() < 2 {
        return None;
    }
    for i in 0..buf.len() - 1 {
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
}
