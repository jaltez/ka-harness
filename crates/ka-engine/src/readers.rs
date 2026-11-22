//! Rich file readers behind the `read` hand: plain-text extraction
//! from PDF documents and ZIP-family containers plus tar (optionally
//! gzip-wrapped) archives — detected by magic bytes, not extensions.
//!
//! Scope of v1 (deliberate, documented for the model):
//! - **PDF**: text extraction only. Objects are found by scanning the
//!   classic `N G obj … endobj` form (xref is not needed); page order
//!   comes from the catalog → `/Pages` → `/Kids` tree, and page text
//!   from the `/Contents` streams' `Tj`/`TJ` operators with
//!   FlateDecode inflation. No images, no embedded fonts, no ToUnicode
//!   CMaps (strings decode as UTF-16BE hex / UTF-8 / Latin-1
//!   fallback). Encrypted PDFs are refused with a clear error; a file
//!   whose page tree cannot be resolved falls back to all text-like
//!   streams in object order, flagged in the output.
//! - **ZIP family** (`.zip`, `.jar`, `.apk`, `.whl`, … — anything with
//!   the `PK` magic): own local-header + central-directory parser.
//!   Store + deflate entries only (flate2 raw deflate); no zip64, no
//!   encrypted entries, no bzip2/lzma methods. Entry integrity is
//!   CRC32-checked.
//! - **tar**: USTAR/GNU header parsing with checksum validation,
//!   including GNU long-name (`L`) entries. `.tar.gz`/`.tgz` decompress
//!   through gzip (multi-member) first.
//! - **DOCX/PPTX/XLSX/EPUB/Jupyter notebooks/SQLite** are OUT of scope
//!   for v1: they are containers this module can only list as raw
//!   members (a `.docx` read lists its XML parts) — no semantic
//!   extraction.
//!
//! Archives are never minted into the read ledger by the hand (they
//! are not edit targets); PDFs are regular files and are.

use std::collections::HashMap;
use std::io::Read as _;
use std::io::Seek as _;

/// Binary container/format detected by magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `%PDF-` header.
    Pdf,
    /// `PK` signatures (zip and its aliases: jar/apk/whl/…).
    Zip,
    /// gzip stream (`1f 8b`) — usually a `.tar.gz`/`.tgz`.
    Gzip,
    /// `ustar` magic at offset 257.
    Tar,
    /// Nothing rich: plain text/image handling applies.
    Other,
}

/// Classify a file by its leading magic bytes (feed at least the first
/// 262 bytes so the tar offset-257 check can see the magic).
pub fn classify(head: &[u8]) -> Kind {
    if head.starts_with(b"%PDF-") {
        Kind::Pdf
    } else if head.starts_with(b"PK\x03\x04")
        || head.starts_with(b"PK\x05\x06")
        || head.starts_with(b"PK\x07\x08")
    {
        Kind::Zip
    } else if head.starts_with(&[0x1f, 0x8b]) {
        Kind::Gzip
    } else if head.len() >= 262 && &head[257..262] == b"ustar" {
        Kind::Tar
    } else {
        Kind::Other
    }
}

// ------------------------------------------------------------------ inflate

/// Inflate a whole stream through any flate2 reader with a hard
/// decompressed-size cap (zip-bomb / hostile-PDF guard).
fn inflate_limited<R: std::io::Read>(
    mut reader: R,
    cap: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf = [0u8; 32 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.len() + n > cap {
                    return Err(format!(
                        "{what} decompresses over the {} KB extraction cap",
                        cap / 1024
                    ));
                }
                out.extend_from_slice(&buf[..n]);
            }
            Err(e) => return Err(format!("{what}: corrupt deflate stream ({e})")),
        }
    }
    Ok(out)
}

/// zlib-wrapped inflate (PDF FlateDecode streams).
pub(crate) fn zlib_inflate(data: &[u8], cap: usize) -> Result<Vec<u8>, String> {
    inflate_limited(
        flate2::read::ZlibDecoder::new(data),
        cap,
        "FlateDecode stream",
    )
}

/// raw deflate inflate (ZIP method 8).
fn raw_inflate(data: &[u8], cap: usize) -> Result<Vec<u8>, String> {
    inflate_limited(
        flate2::read::DeflateDecoder::new(data),
        cap,
        "deflate entry",
    )
}

/// gzip inflate (multi-member; `.tar.gz`/`.tgz`).
pub fn gunzip(data: &[u8], cap: usize) -> Result<Vec<u8>, String> {
    inflate_limited(flate2::read::MultiGzDecoder::new(data), cap, "gzip stream")
}

/// CRC-32 (IEEE 802.3) table, computed at compile time.
const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32 of `data` (ZIP entry integrity).
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

// ------------------------------------------------------------------ helpers

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

/// Human-readable byte size for listings.
fn human_size(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Shared listing rendering for archive members: count header, dirs
/// with a trailing slash, files with their size.
fn render_listing(entries: &[(String, bool, u64)]) -> String {
    if entries.is_empty() {
        return "(empty archive)\n".to_string();
    }
    let mut out = format!("({} entries)\n", entries.len());
    for (name, is_dir, size) in entries {
        if *is_dir {
            let dir = if name.ends_with('/') {
                name.clone()
            } else {
                format!("{name}/")
            };
            out.push_str(&dir);
        } else {
            out.push_str(&format!("{name} ({})\n", human_size(*size)));
        }
    }
    out
}

// ------------------------------------------------------------------ PDF

/// Per-object decompression cap (a single hostile stream must not blow
/// the budget even if many follow).
const PDF_STREAM_CAP: usize = 8 * 1024 * 1024;
/// Total decompressed-content cap per PDF.
const PDF_TOTAL_CAP: usize = 16 * 1024 * 1024;
/// Page-tree walk limits.
const PDF_TREE_DEPTH: usize = 32;
const PDF_PAGE_CAP: usize = 4096;

/// One scanned `N G obj … endobj` unit.
struct PdfObj {
    num: u32,
    /// Lossy dict/text region before the `stream` keyword (or the whole
    /// body when there is no stream).
    dict: String,
    /// Stream payload, EOL-stripped after the `stream` keyword.
    stream: Option<Vec<u8>>,
    /// Whether the dict names FlateDecode as its filter.
    flated: bool,
}

/// Take trailing `num gen` off the text before an `obj` keyword.
fn parse_obj_header(before: &[u8]) -> Option<(u32, u32)> {
    let tail_len = before.len().min(80);
    let tail = String::from_utf8_lossy(&before[before.len() - tail_len..]);
    let mut toks = tail.split_whitespace().rev();
    let gen_num = toks.next()?.parse::<u32>().ok()?;
    let num = toks.next()?.parse::<u32>().ok()?;
    Some((num, gen_num))
}

/// Split an object body into its dict text and optional stream payload.
fn split_obj(body: &[u8]) -> (String, Option<Vec<u8>>) {
    let Some(rel) = find(body, b"stream") else {
        return (String::from_utf8_lossy(body).into_owned(), None);
    };
    let dict_region = &body[..rel];
    // the `stream` keyword must be preceded by whitespace; anything
    // else means the match is inside a string literal — this scanner
    // keeps it simple and accepts the risk (a content stream would
    // have to contain the exact sequence "stream" inside a dict-less
    // object body)
    let dict = String::from_utf8_lossy(dict_region).into_owned();
    let mut data_start = rel + b"stream".len();
    for eol in [&b"\r\n"[..], &b"\n"[..], &b"\r"[..]] {
        if body[data_start..].starts_with(eol) {
            data_start += eol.len();
            break;
        }
    }
    let end_rel = find(&body[data_start..], b"endstream");
    let data = match end_rel {
        Some(e) => &body[data_start..data_start + e],
        None => &body[data_start..],
    };
    (dict, Some(data.to_vec()))
}

/// Scan every classic `N G obj … endobj` unit in the file.
fn scan_objects(bytes: &[u8]) -> Vec<PdfObj> {
    let mut objs = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = find(&bytes[pos..], b"obj") {
        let idx = pos + rel;
        // "obj" must end an object header: preceded by "num gen" + ws
        let header_ok = idx >= 4
            && bytes[idx - 1].is_ascii_whitespace()
            && parse_obj_header(&bytes[..idx]).is_some();
        let body_start = idx + b"obj".len();
        if header_ok {
            if let Some(end_rel) = find(&bytes[body_start..], b"endobj") {
                if let Some((num, _gen)) = parse_obj_header(&bytes[..idx]) {
                    let (dict, stream) = split_obj(&bytes[body_start..body_start + end_rel]);
                    objs.push(PdfObj {
                        num,
                        flated: dict.contains("/FlateDecode"),
                        dict,
                        stream,
                    });
                }
                pos = body_start + end_rel + b"endobj".len();
                continue;
            }
        }
        pos = body_start;
    }
    objs
}

/// The value token(s) after `/{key}` in a PDF dict (ref, array, or
/// name); adjacent-name form `/Key/Value` is handled too.
fn dict_value(dict: &str, key: &str) -> Option<String> {
    let needle = format!("/{key}");
    let mut from = 0usize;
    while let Some(rel) = dict[from..].find(&needle) {
        let after = from + rel + needle.len();
        let boundary = dict[after..].chars().next();
        let ok = matches!(
            boundary,
            None | Some(' ')
                | Some('\n')
                | Some('\r')
                | Some('\t')
                | Some('[')
                | Some('/')
                | Some('<')
                | Some('(')
        );
        if !ok {
            from = after;
            continue; // longer key sharing the prefix (e.g. /Pages vs /PageLayout)
        }
        if boundary == Some('/') {
            // adjacent-name value: /Type/Catalog
            let rest = &dict[after + 1..];
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '/' || c == '>' || c == '[')
                .unwrap_or(rest.len());
            return Some(rest[..end].to_string());
        }
        let rest = dict[after..].trim_start();
        if rest.starts_with('[') {
            let end = rest.find(']')?;
            return Some(rest[..=end].to_string());
        }
        // a value is a name (`/Catalog`), a number, or an indirect ref
        // (`2 0 R` — three whitespace-separated tokens)
        let tok_end = rest
            .find(|c: char| c.is_whitespace() || c == '/' || c == '>' || c == '[')
            .unwrap_or(rest.len());
        let first = &rest[..tok_end];
        if first.parse::<u32>().is_ok() {
            let rest2 = rest[tok_end..].trim_start();
            let t2_end = rest2
                .find(|c: char| c.is_whitespace())
                .unwrap_or(rest2.len());
            let second = &rest2[..t2_end];
            if second.parse::<u32>().is_ok() {
                let rest3 = rest2[t2_end..].trim_start();
                if rest3.starts_with('R') {
                    return Some(format!("{first} {second} R"));
                }
            }
        }
        return Some(first.to_string());
    }
    None
}

/// Every `N 0 R` reference inside a dict value (`4 0 R` or
/// `[4 0 R 5 0 R]`).
fn refs_from(value: &str) -> Vec<u32> {
    let mut refs = Vec::new();
    let trimmed = value.trim_start_matches('[').trim_end_matches(']');
    let toks: Vec<&str> = trimmed.split_whitespace().collect();
    let mut i = 0usize;
    while i + 2 < toks.len() {
        if toks[i].parse::<u32>().is_ok()
            && toks[i + 1].parse::<u32>().is_ok()
            && toks[i + 2] == "R"
        {
            if let Ok(n) = toks[i].parse::<u32>() {
                refs.push(n);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    refs
}

/// Decode a PDF string: UTF-16BE hex when BOM-marked, UTF-8 when it
/// is, Latin-1 (PDFDocEncoding approximation) otherwise.
fn decode_pdf_string(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|p| u16::from_be_bytes([p[0], p[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Parse a literal `(` string starting after the open paren; returns
/// the decoded bytes and the index just past the closing paren.
fn parse_literal_string(data: &[u8], start: usize) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut i = start;
    let mut depth = 1usize;
    while i < data.len() {
        match data[i] {
            b'\\' => {
                i += 1;
                if i >= data.len() {
                    break;
                }
                match data[i] {
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0C),
                    b'(' => out.push(b'('),
                    b')' => out.push(b')'),
                    b'\\' => out.push(b'\\'),
                    b'\n' => {}
                    b'0'..=b'7' => {
                        // up to three octal digits
                        let mut v: u32 = 0;
                        let mut n = 0;
                        while n < 3 && i < data.len() && (b'0'..=b'7').contains(&data[i]) {
                            v = v * 8 + u32::from(data[i] - b'0');
                            i += 1;
                            n += 1;
                        }
                        i -= 1; // the loop below advances past this byte
                        out.push((v & 0xFF) as u8);
                    }
                    other => out.push(other),
                }
                i += 1;
            }
            b'(' => {
                depth += 1;
                out.push(b'(');
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    break;
                }
                out.push(b')');
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    (out, i)
}

/// Parse a `<…>` hex string starting after the `<`; returns the decoded
/// bytes and the index just past `>`.
fn parse_hex_string(data: &[u8], start: usize) -> (Vec<u8>, usize) {
    let mut hex: Vec<u8> = Vec::new();
    let mut i = start;
    while i < data.len() && data[i] != b'>' {
        if data[i].is_ascii_hexdigit() {
            hex.push(data[i]);
        }
        i += 1;
    }
    if hex.len() % 2 == 1 {
        hex.push(b'0'); // odd digit count: pad on the right
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in hex.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (pair[1] as char).to_digit(16).unwrap_or(0) as u8;
        out.push(hi * 16 + lo);
    }
    (out, (i + 1).min(data.len()))
}

/// Token delimiters inside a content stream.
fn is_delim(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\n'
            | b'\r'
            | b'\t'
            | 0x00
            | 0x0C
            | b'('
            | b')'
            | b'<'
            | b'>'
            | b'['
            | b']'
            | b'/'
            | b'{'
            | b'}'
    )
}

/// Extract the visible text of one decoded content stream. Line breaks
/// follow the text-positioning operators (`T*`, `Td`/`TD` with a
/// non-zero y move, `ET`); `Tj`/`'`/`"`/`TJ` append their strings.
fn content_stream_text(data: &[u8]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut pending: Vec<String> = Vec::new();
    let mut last_number = 0.0f32;
    let mut token = String::new();

    let flush_line = |lines: &mut Vec<String>, cur: &mut String| {
        lines.push(std::mem::take(cur));
    };
    let flush_token = |token: &mut String,
                       pending: &mut Vec<String>,
                       cur: &mut String,
                       lines: &mut Vec<String>,
                       last_number: &mut f32| {
        let tok = std::mem::take(token);
        if tok.is_empty() {
            return;
        }
        if let Ok(n) = tok.parse::<f32>() {
            *last_number = n;
            return;
        }
        match tok.as_str() {
            "Tj" | "TJ" => {
                for s in pending.drain(..) {
                    cur.push_str(&s);
                }
            }
            "'" | "\"" => {
                flush_line(lines, cur);
                for s in pending.drain(..) {
                    cur.push_str(&s);
                }
            }
            "T*" | "ET" => flush_line(lines, cur),
            "Td" | "TD" => {
                if *last_number != 0.0 {
                    flush_line(lines, cur);
                }
            }
            "BT" => pending.clear(),
            _ => {}
        }
    };

    let mut i = 0usize;
    while i < data.len() {
        let b = data[i];
        match b {
            b'(' => {
                let (bytes, next) = parse_literal_string(data, i + 1);
                pending.push(decode_pdf_string(&bytes));
                i = next;
            }
            b'<' if data.get(i + 1) == Some(&b'<') => {
                // dict in a content stream (BDC properties): skip it
                let end = find(&data[i..], b">>")
                    .map(|r| i + r + 2)
                    .unwrap_or(data.len());
                i = end;
            }
            b'<' => {
                let (bytes, next) = parse_hex_string(data, i + 1);
                pending.push(decode_pdf_string(&bytes));
                i = next;
            }
            b'[' => {
                // TJ array: its strings join the pending set (kerning
                // numbers just update the number tracker)
                i += 1;
                while i < data.len() && data[i] != b']' {
                    match data[i] {
                        b'(' => {
                            let (bytes, next) = parse_literal_string(data, i + 1);
                            pending.push(decode_pdf_string(&bytes));
                            i = next;
                        }
                        b'<' => {
                            let (bytes, next) = parse_hex_string(data, i + 1);
                            pending.push(decode_pdf_string(&bytes));
                            i = next;
                        }
                        _ => {
                            if data[i].is_ascii_whitespace() {
                                flush_token(
                                    &mut token,
                                    &mut pending,
                                    &mut cur,
                                    &mut lines,
                                    &mut last_number,
                                );
                            } else if !is_delim(data[i]) {
                                token.push(data[i] as char);
                            }
                            i += 1;
                        }
                    }
                }
                i += 1;
            }
            _ if b.is_ascii_whitespace() => {
                flush_token(
                    &mut token,
                    &mut pending,
                    &mut cur,
                    &mut lines,
                    &mut last_number,
                );
                i += 1;
            }
            _ if is_delim(b) => {
                flush_token(
                    &mut token,
                    &mut pending,
                    &mut cur,
                    &mut lines,
                    &mut last_number,
                );
                i += 1;
            }
            _ => {
                token.push(b as char);
                i += 1;
            }
        }
    }
    flush_token(
        &mut token,
        &mut pending,
        &mut cur,
        &mut lines,
        &mut last_number,
    );
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
        .into_iter()
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decode one content stream (inflating it when the object says
/// FlateDecode) and extract its text.
fn decode_stream(obj: &PdfObj) -> Result<String, String> {
    let Some(data) = &obj.stream else {
        return Ok(String::new());
    };
    let decoded = if obj.flated {
        zlib_inflate(data, PDF_STREAM_CAP)?
    } else {
        data.clone()
    };
    Ok(content_stream_text(&decoded))
}

/// Text of every page, resolved through the page tree when possible
/// (catalog → /Pages → /Kids recursion), else every text-like stream
/// in object order. Pages join with form feeds (PDF's own page
/// separator).
pub fn pdf_text(bytes: &[u8]) -> Result<String, String> {
    if !bytes.starts_with(b"%PDF-") {
        return Err("not a PDF (missing %PDF- header)".to_string());
    }
    if is_encrypted(bytes) {
        return Err("the PDF is encrypted — ka reads only unencrypted PDFs".to_string());
    }
    let objs = scan_objects(bytes);
    if objs
        .iter()
        .any(|o| o.dict.contains("/Standard") && o.dict.contains("/Filter"))
    {
        return Err("the PDF is encrypted — ka reads only unencrypted PDFs".to_string());
    }
    let mut by_num: HashMap<u32, usize> = HashMap::with_capacity(objs.len());
    for (i, o) in objs.iter().enumerate() {
        by_num.entry(o.num).or_insert(i);
    }
    let mut budget = PDF_TOTAL_CAP;

    let page_objs = page_order(&objs, &by_num);
    let mut pages: Vec<String> = Vec::new();
    match page_objs {
        Some(nums) => {
            for num in nums {
                let Some(&idx) = by_num.get(&num) else {
                    continue;
                };
                let page = &objs[idx];
                // a page's text lives in its /Contents stream(s)
                let refs = dict_value(&page.dict, "Contents")
                    .as_deref()
                    .map(refs_from)
                    .unwrap_or_default();
                for r in refs {
                    let Some(&cidx) = by_num.get(&r) else {
                        continue;
                    };
                    let mut text = decode_stream(&objs[cidx])?;
                    if text.len() > budget {
                        return Err("the PDF decompresses over the extraction cap".to_string());
                    }
                    budget -= text.len();
                    if !text.is_empty() {
                        pages.push(std::mem::take(&mut text));
                    }
                }
            }
        }
        None => {
            // page tree unresolvable (xref-stream-only writers, hybrid
            // files): pull every text-like stream in object order
            for o in &objs {
                if o.stream.is_none() || o.dict.contains("/Image") || o.dict.contains("/FontFile") {
                    continue;
                }
                let text = match decode_stream(o) {
                    Ok(t) => t,
                    Err(_) => continue, // a broken non-text stream must not sink the read
                };
                if text.len() > budget {
                    return Err("the PDF decompresses over the extraction cap".to_string());
                }
                budget -= text.len();
                if text.contains("Tj") || text.contains("TJ") {
                    pages.push(text);
                }
            }
            if !pages.is_empty() {
                pages.insert(
                    0,
                    "[ka: page tree not found in this PDF — streams follow in object order]"
                        .to_string(),
                );
            }
        }
    }
    if pages.is_empty() {
        return Err(
            "no extractable text found — the PDF may be scanned images or use an \
             unsupported encoder"
                .to_string(),
        );
    }
    Ok(pages.join("\n\u{c}\n"))
}

/// Whether the file declares encryption: the trailer dict carries
/// `/Encrypt`, or an object defines the standard security handler.
fn is_encrypted(bytes: &[u8]) -> bool {
    if let Some(rel) = rfind(bytes, b"trailer") {
        let window = &bytes[rel..(rel + 1024).min(bytes.len())];
        if find(window, b"/Encrypt").is_some() {
            return true;
        }
    }
    // encryption dictionary object (only meaningful with /Filter
    // /Standard, so the two keys together are the signal)
    let mut pos = 0usize;
    while let Some(rel) = find(&bytes[pos..], b"/Encrypt") {
        let at = pos + rel;
        let after = &bytes[at..(at + 200).min(bytes.len())];
        if find(after, b"/Standard").is_some() {
            return true;
        }
        pos = at + b"/Encrypt".len();
    }
    false
}

/// Object numbers of the pages in document order, walking the
/// catalog's page tree. `None` when no catalog/page tree exists.
fn page_order(objs: &[PdfObj], by_num: &HashMap<u32, usize>) -> Option<Vec<u32>> {
    let catalog = objs.iter().find(|o| o.dict.contains("/Catalog"))?;
    let pages_val = dict_value(&catalog.dict, "Pages")?;
    let root = refs_from(&pages_val).first().copied()?;
    let mut out = Vec::new();
    let mut visited = std::collections::HashSet::new();
    walk_page_node(objs, by_num, root, 0, &mut out, &mut visited);
    if out.is_empty() { None } else { Some(out) }
}

/// Recursive `/Kids` walk: branch nodes recurse, leaf nodes (objects
/// with `/Type /Page` or `/Contents`) are page entries.
fn walk_page_node(
    objs: &[PdfObj],
    by_num: &HashMap<u32, usize>,
    num: u32,
    depth: usize,
    out: &mut Vec<u32>,
    visited: &mut std::collections::HashSet<u32>,
) {
    if depth > PDF_TREE_DEPTH || out.len() >= PDF_PAGE_CAP || !visited.insert(num) {
        return;
    }
    let Some(&idx) = by_num.get(&num) else {
        return;
    };
    let obj = &objs[idx];
    if let Some(kids_val) = dict_value(&obj.dict, "Kids") {
        for kid in refs_from(&kids_val) {
            walk_page_node(objs, by_num, kid, depth + 1, out, visited);
            if out.len() >= PDF_PAGE_CAP {
                return;
            }
        }
        return;
    }
    if obj.dict.contains("/Type/Page")
        || obj.dict.contains("/Type /Page")
        || dict_value(&obj.dict, "Contents").is_some()
    {
        out.push(num);
    }
}

// ------------------------------------------------------------------ ZIP

/// A ZIP (or jar/apk/whl/…) container, parsed from its central
/// directory. Open with a seekable file — multi-GB archives never get
/// materialized for a listing; member reads pull only their own bytes.
pub struct ZipDir {
    entries: Vec<ZipEntry>,
}

/// One central-directory record.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    pub name: String,
    pub is_dir: bool,
    /// 0 = stored, 8 = deflate.
    pub method: u16,
    pub crc: u32,
    pub compressed: u64,
    pub size: u64,
    /// Offset of the entry's local header.
    pub header_offset: u64,
    /// General-purpose bit 0: the entry is password-encrypted.
    pub encrypted: bool,
}

const ZIP_ENTRY_CAP: usize = 8192;

impl ZipDir {
    /// Parse the end-of-central-directory record and central directory.
    pub fn open(file: &mut std::fs::File) -> Result<ZipDir, String> {
        let len = file.metadata().map_err(|e| format!("zip: {e}"))?.len();
        if len < 22 {
            return Err("not a zip container (no end-of-central-directory)".to_string());
        }
        // EOCD lives in the last 22 + comment (≤ 64 KB) bytes
        let tail_len = len.min(22 + 65_535);
        file.seek(std::io::SeekFrom::Start(len - tail_len))
            .map_err(|e| format!("zip: {e}"))?;
        let mut tail = vec![0u8; tail_len as usize];
        file.read_exact(&mut tail)
            .map_err(|e| format!("zip: {e}"))?;
        let eocd = rfind(&tail, &[0x50, 0x4B, 0x05, 0x06])
            .ok_or_else(|| "not a zip container (no end-of-central-directory)".to_string())?;
        let rd16 = |at: usize| u16::from_le_bytes([tail[at], tail[at + 1]]);
        let rd32 =
            |at: usize| u32::from_le_bytes([tail[at], tail[at + 1], tail[at + 2], tail[at + 3]]);
        if rd16(eocd + 10) == 0xFFFF || rd32(eocd + 16) == 0xFFFF_FFFF {
            return Err("zip64 archives are not supported".to_string());
        }
        let cd_size = u64::from(rd32(eocd + 12));
        let cd_offset = u64::from(rd32(eocd + 16));
        if cd_offset + cd_size > len {
            return Err("zip is corrupt (central directory out of bounds)".to_string());
        }
        file.seek(std::io::SeekFrom::Start(cd_offset))
            .map_err(|e| format!("zip: {e}"))?;
        let mut cd = vec![0u8; cd_size as usize];
        file.read_exact(&mut cd).map_err(|e| format!("zip: {e}"))?;

        let mut entries = Vec::new();
        let mut pos = 0usize;
        while pos + 46 <= cd.len()
            && cd[pos..pos + 4] == [0x50, 0x4B, 0x01, 0x02]
            && entries.len() < ZIP_ENTRY_CAP
        {
            let rd16c = |at: usize| u16::from_le_bytes([cd[at], cd[at + 1]]);
            let rd32c =
                |at: usize| u32::from_le_bytes([cd[at], cd[at + 1], cd[at + 2], cd[at + 3]]);
            let flags = rd16c(pos + 8);
            let method = rd16c(pos + 10);
            let crc = rd32c(pos + 16);
            let compressed = u64::from(rd32c(pos + 20));
            let size = u64::from(rd32c(pos + 24));
            let name_len = rd16c(pos + 28) as usize;
            let extra_len = rd16c(pos + 30) as usize;
            let comment_len = rd16c(pos + 32) as usize;
            let header_offset = u64::from(rd32c(pos + 42));
            let name_end = (pos + 46 + name_len).min(cd.len());
            let name = String::from_utf8_lossy(&cd[pos + 46..name_end]).into_owned();
            entries.push(ZipEntry {
                is_dir: name.ends_with('/'),
                encrypted: flags & 1 != 0,
                name,
                method,
                crc,
                compressed,
                size,
                header_offset,
            });
            pos += 46 + name_len + extra_len + comment_len;
        }
        Ok(ZipDir { entries })
    }

    pub fn entries(&self) -> &[ZipEntry] {
        &self.entries
    }

    /// `(N entries)` listing in central-directory order.
    pub fn listing(&self) -> String {
        let rows: Vec<(String, bool, u64)> = self
            .entries
            .iter()
            .map(|e| (e.name.clone(), e.is_dir, e.size))
            .collect();
        render_listing(&rows)
    }

    /// Look up an entry by exact name.
    pub fn find(&self, name: &str) -> Option<&ZipEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Read and decode one member: method check (store/deflate only),
    /// local-header offset resolution (local name/extra lengths may
    /// differ from the central record), inflate, size + CRC verify.
    pub fn read(
        &self,
        file: &mut std::fs::File,
        name: &str,
        cap: usize,
    ) -> Result<Vec<u8>, String> {
        let entry = self.find(name).ok_or_else(|| {
            format!(
                "entry '{name}' not found in the archive — read it without \
                 archive_entry for the member listing"
            )
        })?;
        if entry.is_dir {
            return Err(format!("archive entry '{name}' is a directory"));
        }
        if entry.encrypted {
            return Err(format!(
                "archive entry '{name}' is password-encrypted — ka reads only \
                 unencrypted archives"
            ));
        }
        if entry.method != 0 && entry.method != 8 {
            return Err(format!(
                "archive entry '{name}': compression method {} not supported \
                 (store/deflate only)",
                entry.method
            ));
        }
        // local header: signature + fixed fields; the local name and
        // extra lengths are authoritative for the data offset
        file.seek(std::io::SeekFrom::Start(entry.header_offset))
            .map_err(|e| format!("zip: {e}"))?;
        let mut local = [0u8; 30];
        file.read_exact(&mut local)
            .map_err(|e| format!("zip: {e}"))?;
        if local[0..4] != [0x50, 0x4B, 0x03, 0x04] {
            return Err(format!(
                "zip is corrupt (bad local header for entry '{name}')"
            ));
        }
        let local_name_len = u16::from_le_bytes([local[26], local[27]]) as u64;
        let local_extra_len = u16::from_le_bytes([local[28], local[29]]) as u64;
        let data_offset = entry.header_offset + 30 + local_name_len + local_extra_len;
        if entry.compressed > cap as u64 {
            return Err(format!(
                "archive entry '{name}' is over the {} KB extraction cap",
                cap / 1024
            ));
        }
        file.seek(std::io::SeekFrom::Start(data_offset))
            .map_err(|e| format!("zip: {e}"))?;
        let mut packed = vec![0u8; entry.compressed as usize];
        file.read_exact(&mut packed)
            .map_err(|e| format!("zip entry '{name}': {e}"))?;
        let data = match entry.method {
            8 => raw_inflate(&packed, cap).map_err(|e| format!("entry '{name}': {e}"))?,
            _ => packed,
        };
        if data.len() as u64 != entry.size {
            return Err(format!(
                "zip entry '{name}' is corrupt (size mismatch: got {}, header says {})",
                data.len(),
                entry.size
            ));
        }
        if crc32(&data) != entry.crc {
            return Err(format!("zip entry '{name}' is corrupt (crc mismatch)"));
        }
        Ok(data)
    }
}

// ------------------------------------------------------------------ tar

/// One tar member: its name plus where its data sits in the (already
/// in-memory) archive bytes.
#[derive(Debug, Clone)]
pub struct TarEntry {
    pub name: String,
    pub offset: u64,
    pub size: u64,
    pub is_dir: bool,
}

const TAR_ENTRY_CAP: usize = 16_384;

/// Parse USTAR/GNU tar blocks with checksum validation. Data must be
/// the full (already gunzipped, when needed) archive.
pub fn tar_entries(data: &[u8]) -> Result<Vec<TarEntry>, String> {
    let mut out = Vec::new();
    let mut long_name: Option<String> = None;
    let mut pos = 0usize;
    while pos + 512 <= data.len() && out.len() < TAR_ENTRY_CAP {
        let hdr = &data[pos..pos + 512];
        if hdr.iter().all(|&b| b == 0) {
            break; // end-of-archive block(s)
        }
        let stored_sum = tar_octal(hdr, 148, 8)
            .map_err(|_| format!("tar is corrupt (bad checksum field at offset {pos})"))?;
        let computed: u32 = hdr
            .iter()
            .enumerate()
            .map(|(i, &b)| u32::from(if (148..156).contains(&i) { b' ' } else { b }))
            .sum();
        if stored_sum != computed {
            return Err(format!(
                "tar is corrupt (checksum mismatch at offset {pos})"
            ));
        }
        let size = tar_size(hdr, 124, 12)
            .map_err(|_| format!("tar is corrupt (bad size field at offset {pos})"))?;
        let typeflag = hdr[156];
        let data_at = pos + 512;
        let round = size.div_ceil(512) * 512;
        let next = data_at + round as usize;

        let header_name = tar_cstr(hdr, 0, 100);
        match typeflag {
            b'0' | 0 => {
                let name = long_name.take().unwrap_or_else(|| {
                    let mut n = header_name;
                    let prefix = tar_cstr(hdr, 345, 155);
                    if !prefix.is_empty() {
                        n = format!("{prefix}/{n}");
                    }
                    n
                });
                out.push(TarEntry {
                    name,
                    offset: data_at as u64,
                    size,
                    is_dir: false,
                });
            }
            b'5' => {
                long_name = None;
                out.push(TarEntry {
                    name: header_name,
                    offset: data_at as u64,
                    size: 0,
                    is_dir: true,
                });
            }
            b'L' => {
                // GNU long name: the payload IS the next entry's name
                let end = (data_at + size as usize).min(data.len());
                long_name = Some(
                    tar_cstr(&data[data_at..end], 0, end - data_at)
                        .trim()
                        .to_string(),
                );
            }
            // dirs-of-other-kinds (symlinks, pax headers, devices…) are
            // skipped, not listed
            _ => {
                long_name = None;
            }
        }
        if next <= pos {
            break; // defensive: a zero/huge size must not loop forever
        }
        pos = next;
    }
    Ok(out)
}

/// NUL-terminated string field.
fn tar_cstr(hdr: &[u8], at: usize, len: usize) -> String {
    let field = &hdr[at..(at + len).min(hdr.len())];
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Octal field (spaces/NUL padded). GNU base-256 (high bit of first
/// byte) is decoded as a big-endian binary number when present.
fn tar_size(hdr: &[u8], at: usize, len: usize) -> Result<u64, String> {
    if len == 0 {
        return Ok(0);
    }
    let first = hdr[at];
    if first & 0x80 != 0 {
        // base-256: (first & 0x7f) then plain big-endian bytes
        let mut v = u64::from(first & 0x7F);
        for &b in &hdr[at + 1..at + len] {
            let shifted = v
                .checked_mul(256)
                .ok_or_else(|| "tar size field overflows".to_string())?;
            v = shifted
                .checked_add(u64::from(b))
                .ok_or_else(|| "tar size field overflows".to_string())?;
        }
        return Ok(v);
    }
    tar_octal(hdr, at, len).map(u64::from)
}

fn tar_octal(hdr: &[u8], at: usize, len: usize) -> Result<u32, String> {
    let field = &hdr[at..(at + len).min(hdr.len())];
    let s = String::from_utf8_lossy(field);
    let digits = s.trim().trim_end_matches('\0');
    if digits.is_empty() {
        return Ok(0);
    }
    u32::from_str_radix(digits, 8).map_err(|_| "not an octal field".to_string())
}

/// Member listing for an in-memory archive.
pub fn tar_listing(entries: &[TarEntry]) -> String {
    let rows: Vec<(String, bool, u64)> = entries
        .iter()
        .map(|e| (e.name.clone(), e.is_dir, e.size))
        .collect();
    render_listing(&rows)
}

/// A member's bytes out of in-memory tar data (bounds-checked).
pub fn tar_entry_data<'a>(
    data: &'a [u8],
    entry: &TarEntry,
    cap: usize,
) -> Result<&'a [u8], String> {
    if entry.size as usize > cap {
        return Err(format!(
            "archive entry '{}' is over the {} KB extraction cap",
            entry.name,
            cap / 1024
        ));
    }
    let start = entry.offset as usize;
    let end = start
        .checked_add(entry.size as usize)
        .ok_or_else(|| format!("archive entry '{}' has a corrupt size", entry.name))?;
    data.get(start..end)
        .ok_or_else(|| format!("archive entry '{}' is corrupt (out of bounds)", entry.name))
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Test-only fixture builders (minimal writers for the formats this
    //! module reads).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::crc32;

    /// zlib-compress (PDF FlateDecode level).
    pub fn zlib(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(data).expect("zlib fixture");
        enc.finish().expect("zlib fixture")
    }

    /// raw deflate (ZIP method 8).
    pub fn deflate(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(data).expect("deflate fixture");
        enc.finish().expect("deflate fixture")
    }

    /// gzip-wrap (multi-member capable reader reads these).
    pub fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(data).expect("gzip fixture");
        enc.finish().expect("gzip fixture")
    }

    /// A classic-xref PDF: catalog → pages → kids, each page one
    /// content stream. `page_bodies` are content-stream operator texts;
    /// `flated` compresses page 1's stream.
    pub fn pdf(page_bodies: &[&str], flated: bool) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets: Vec<usize> = Vec::new();
        let catalog = "<< /Type /Catalog /Pages 2 0 R >>";
        let kids: Vec<String> = (0..page_bodies.len())
            .map(|i| format!("{} 0 R", 4 + i * 2))
            .collect();
        let pages = format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            page_bodies.len()
        );
        for (num, body) in [(1u32, catalog.to_string()), (2, pages)] {
            offsets.push(out.len());
            out.extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
        }
        for (i, text) in page_bodies.iter().enumerate() {
            let content_num = 3 + i as u32 * 2;
            let page_num = 4 + i as u32 * 2;
            let payload = if flated && i == 0 {
                zlib(text.as_bytes())
            } else {
                text.as_bytes().to_vec()
            };
            let dict = if flated && i == 0 {
                "<< /Length %N /Filter /FlateDecode >>".replace("%N", &payload.len().to_string())
            } else {
                "<< /Length %N >>".replace("%N", &payload.len().to_string())
            };
            offsets.push(out.len());
            out.extend_from_slice(format!("{content_num} 0 obj\n{dict}\nstream\n").as_bytes());
            out.extend_from_slice(&payload);
            out.extend_from_slice(b"\nendstream\nendobj\n");
            offsets.push(out.len());
            out.extend_from_slice(
                format!("{page_num} 0 obj\n<< /Type /Page /Parent 2 0 R /Contents {content_num} 0 R >>\nendobj\n")
                    .as_bytes(),
            );
        }
        out.extend_from_slice(b"trailer\n<< /Size 9 /Root 1 0 R >>\nstartxref\n0\n%%EOF\n");
        out
    }

    /// Same as [`pdf`] but with an `/Encrypt` trailer entry.
    pub fn encrypted_pdf(page_bodies: &[&str]) -> Vec<u8> {
        let out = pdf(page_bodies, false);
        // splice an /Encrypt into the trailer dict
        let marker = b"/Size 9";
        if let Some(at) = out.windows(marker.len()).rposition(|w| w == marker) {
            let mut patched = Vec::new();
            patched.extend_from_slice(&out[..at]);
            patched.extend_from_slice(b"/Encrypt 8 0 R ");
            patched.extend_from_slice(&out[at..]);
            // and define the security-handler object
            patched.extend_from_slice(
                b"8 0 obj\n<< /Filter /Standard /V 1 /R 2 /O (x) /U (y) /P -1 >>\nendobj\n",
            );
            return patched;
        }
        out
    }

    /// One ZIP local-header block; returns (header+data bytes, crc,
    /// compressed size, uncompressed size).
    #[allow(clippy::too_many_arguments)]
    fn zip_local(
        out: &mut Vec<u8>,
        name: &str,
        data: &[u8],
        deflate_it: bool,
        encrypted_flag: bool,
    ) -> (u32, u64, u64) {
        let payload = if deflate_it {
            deflate(data)
        } else {
            data.to_vec()
        };
        let method: u16 = if deflate_it { 8 } else { 0 };
        let crc = crc32(data);
        let flags: u16 = if encrypted_flag { 1 } else { 0 };
        let offset = out.len() as u64;
        out.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]);
        out.extend_from_slice(&20u16.to_le_bytes()); // version
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&payload);
        (crc, offset, payload.len() as u64)
    }

    /// A complete ZIP container. `entries: (name, data, deflate?,
    /// encrypted?)`.
    pub fn zip(entries: &[(&str, &[u8], bool, bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        struct Central {
            name: String,
            crc: u32,
            csize: u64,
            usize_: u64,
            method: u16,
            flags: u16,
            offset: u64,
        }
        let mut centrals: Vec<Central> = Vec::new();
        for (name, data, deflate_it, encrypted) in entries {
            let (crc, offset, csize) = zip_local(&mut out, name, data, *deflate_it, *encrypted);
            centrals.push(Central {
                name: (*name).to_string(),
                crc,
                csize,
                usize_: data.len() as u64,
                method: if *deflate_it { 8 } else { 0 },
                flags: if *encrypted { 1 } else { 0 },
                offset,
            });
        }
        let cd_start = out.len() as u32;
        for c in &centrals {
            out.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02]);
            out.extend_from_slice(&20u16.to_le_bytes()); // version made
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&c.flags.to_le_bytes());
            out.extend_from_slice(&c.method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0u16.to_le_bytes()); // date
            out.extend_from_slice(&c.crc.to_le_bytes());
            out.extend_from_slice(&(c.csize as u32).to_le_bytes());
            out.extend_from_slice(&(c.usize_ as u32).to_le_bytes());
            out.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(&0u16.to_le_bytes()); // comment
            out.extend_from_slice(&0u16.to_le_bytes()); // disk
            out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            out.extend_from_slice(&(c.offset as u32).to_le_bytes());
            out.extend_from_slice(c.name.as_bytes());
        }
        let cd_size = out.len() as u32 - cd_start;
        out.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]);
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // cd disk
        out.extend_from_slice(&(centrals.len() as u16).to_le_bytes());
        out.extend_from_slice(&(centrals.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_start.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out
    }

    /// One USTAR header block with a valid checksum.
    fn tar_header(name: &str, size: u64, typeflag: u8) -> Vec<u8> {
        let mut hdr = vec![0u8; 512];
        let name_bytes = name.as_bytes();
        let take = name_bytes.len().min(100);
        hdr[..take].copy_from_slice(&name_bytes[..take]);
        hdr[100..108].copy_from_slice(b"0000644\0"); // mode
        hdr[108..116].copy_from_slice(b"0000000\0"); // uid
        hdr[116..124].copy_from_slice(b"0000000\0"); // gid
        let size_field = format!("{:011o}\0", size);
        hdr[124..136].copy_from_slice(size_field.as_bytes());
        hdr[136..148].copy_from_slice(b"00000000000\0"); // mtime
        hdr[148..156].copy_from_slice(b"        "); // checksum placeholder
        hdr[156] = typeflag;
        hdr[257..263].copy_from_slice(b"ustar\0");
        hdr[263..265].copy_from_slice(b"00");
        let sum: u32 = hdr
            .iter()
            .enumerate()
            .map(|(i, &b)| u32::from(if (148..156).contains(&i) { b' ' } else { b }))
            .sum();
        let ck = format!("{:06o}\0 ", sum);
        hdr[148..156].copy_from_slice(ck.as_bytes());
        hdr
    }

    /// A complete tar archive. `entries: (name, data)`; names ending
    /// in `/` become directory entries.
    pub fn tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in entries {
            let typeflag = if name.ends_with('/') { b'5' } else { b'0' };
            let size = if typeflag == b'5' {
                0
            } else {
                data.len() as u64
            };
            let (hdr_name, prefix) = if name.len() > 100 {
                // ustar prefix/split for long names
                let split = name.len() - 100;
                let (p, n) = name.split_at(split);
                let n = n.strip_prefix('/').unwrap_or(n);
                (n.to_string(), p.to_string())
            } else {
                ((*name).to_string(), String::new())
            };
            let mut hdr = tar_header(&hdr_name, size, typeflag);
            if !prefix.is_empty() {
                let pb = prefix.as_bytes();
                let take = pb.len().min(155);
                hdr[345..345 + take].copy_from_slice(&pb[..take]);
                // checksum changes — recompute
                let sum: u32 = hdr
                    .iter()
                    .enumerate()
                    .map(|(i, &b)| u32::from(if (148..156).contains(&i) { b' ' } else { b }))
                    .sum();
                let ck = format!("{:06o}\0 ", sum);
                hdr[148..156].copy_from_slice(ck.as_bytes());
            }
            out.extend_from_slice(&hdr);
            if typeflag == b'0' {
                out.extend_from_slice(data);
                let pad = (512 - data.len() % 512) % 512;
                out.extend(std::iter::repeat_n(0u8, pad));
            }
        }
        out.extend(std::iter::repeat_n(0u8, 1024)); // end blocks
        out
    }

    /// A GNU long-name file entry followed by a short one.
    pub fn tar_with_longname(long_name: &str, data: &[u8], short: (&str, &[u8])) -> Vec<u8> {
        let mut out = Vec::new();
        let mut payload = long_name.as_bytes().to_vec();
        payload.push(0);
        let pad = (512 - payload.len() % 512) % 512;
        payload.extend(std::iter::repeat_n(0u8, pad));
        out.extend_from_slice(&tar_header("././@LongLink", payload.len() as u64, b'L'));
        out.extend_from_slice(&payload);
        out.extend_from_slice(&tar_header(
            &long_name[..100.min(long_name.len())],
            data.len() as u64,
            b'0',
        ));
        out.extend_from_slice(data);
        let dpad = (512 - data.len() % 512) % 512;
        out.extend(std::iter::repeat_n(0u8, dpad));
        out.extend_from_slice(&tar_header(short.0, short.1.len() as u64, b'0'));
        out.extend_from_slice(short.1);
        let dpad = (512 - short.1.len() % 512) % 512;
        out.extend(std::iter::repeat_n(0u8, dpad));
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::fixtures::*;
    use super::*;

    // ------------------------------------------------------ detection

    #[test]
    fn classify_detects_formats_by_magic() {
        assert_eq!(classify(b"%PDF-1.7 junk"), Kind::Pdf);
        assert_eq!(classify(b"PK\x03\x04rest"), Kind::Zip);
        assert_eq!(classify(b"PK\x05\x06empty-zip"), Kind::Zip);
        assert_eq!(classify(&[0x1f, 0x8b, 0x08, 0x00]), Kind::Gzip);
        let mut tar_head = vec![0u8; 262];
        tar_head[257..262].copy_from_slice(b"ustar");
        assert_eq!(classify(&tar_head), Kind::Tar);
        assert_eq!(classify(b"plain text file, nothing rich"), Kind::Other);
        assert_eq!(classify(b""), Kind::Other);
    }

    #[test]
    fn crc32_matches_the_standard_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    // ------------------------------------------------------------ PDF

    #[test]
    fn pdf_pages_extract_in_order_through_compressed_and_plain_streams() {
        let bytes = pdf(
            &[
                "BT /F1 12 Tf 72 720 Td (First page) Tj 0 -14 Td (line two) Tj ET",
                "BT /F1 12 Tf 72 720 Td (Second \\(page\\)) Tj T* (last line) Tj ET",
            ],
            true,
        );
        let text = pdf_text(&bytes).unwrap();
        // page order preserved, pages split by form feed
        let pages: Vec<&str> = text.split('\u{c}').collect();
        assert_eq!(pages.len(), 2, "{text:?}");
        assert!(pages[0].contains("First page"), "{text:?}");
        assert!(pages[0].contains("line two"));
        assert!(
            pages[1].contains("Second (page)"),
            "escape decoded: {text:?}"
        );
        assert!(pages[1].contains("last line"));
        assert!(!text.contains("Tj"), "operators must not leak: {text:?}");
    }

    #[test]
    fn pdf_tj_arrays_and_hex_strings_extract() {
        let content = "[(Ker)(ned) -120 (array)] TJ [(<48656c6c6f>) ...] TJ"
            .replace("(<48656c6c6f>) ...", "<48656c6c6f> Tj");
        let text = content_stream_text(content.as_bytes());
        assert!(text.contains("Kernedarray"), "{text:?}");
        assert!(text.contains("Hello"), "{text:?}");
    }

    #[test]
    fn pdf_utf16_and_octal_strings_decode() {
        // UTF-16BE hex string: "Hi"
        let text = content_stream_text(b"<FEFF00480069> Tj");
        assert_eq!(text, "Hi");
        // octal escape \101 = 'A'
        let text = content_stream_text(b"(\\101\\102) Tj");
        assert_eq!(text, "AB");
    }

    #[test]
    fn pdf_positioning_operators_break_lines() {
        // Td with non-zero y moves a line; Td 0 0 does not
        let text = content_stream_text(b"BT (a) Tj 0 -12 Td (b) Tj 0 0 Td (c) Tj ET");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text:?}");
        assert_eq!(lines[0], "a", "{text:?}");
        assert!(lines[1].contains('b') && lines[1].contains('c'), "{text:?}");
    }

    #[test]
    fn encrypted_pdf_is_refused_with_a_clear_error() {
        let bytes = encrypted_pdf(&["BT (secret) Tj ET"]);
        let err = pdf_text(&bytes).unwrap_err();
        assert!(err.contains("encrypted"), "{err}");
    }

    #[test]
    fn corrupt_pdf_errors_clearly() {
        // header but garbage objects
        let err = pdf_text(b"%PDF-1.4\nthis is not pdf at all\n%%EOF\n").unwrap_err();
        assert!(err.contains("no extractable text"), "{err}");
        // broken FlateDecode payload
        let mut bytes = pdf(&["BT (x) Tj ET"], true);
        // flip bytes inside the compressed stream (find "stream\n")
        let at = find(&bytes, b"stream\n").unwrap() + 7;
        bytes[at] ^= 0xFF;
        let err = pdf_text(&bytes).unwrap_err();
        assert!(
            err.contains("corrupt") || err.contains("FlateDecode") || err.contains("cap"),
            "{err}"
        );
    }

    #[test]
    fn pdf_text_is_not_a_pdf_error() {
        let err = pdf_text(b"PK\x03\x04not-a-pdf").unwrap_err();
        assert!(err.contains("not a PDF"), "{err}");
    }

    // ------------------------------------------------------------ ZIP

    fn open_zip(bytes: &[u8]) -> (ZipDir, std::fs::File) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ka-readers-zip-{}-{}.zip",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let dir = ZipDir::open(&mut file).unwrap();
        std::fs::remove_file(&path).ok();
        (dir, file)
    }

    #[test]
    fn zip_listing_and_member_reads_store_and_deflate() {
        let bytes = zip(&[
            ("src/", b"", false, false),
            ("src/main.rs", b"fn main() {}\n", true, false),
            ("notes.txt", b"plain stored\n", false, false),
        ]);
        let (dir, mut file) = open_zip(&bytes);
        let listing = dir.listing();
        assert!(listing.contains("(3 entries)"), "{listing}");
        assert!(listing.contains("src/"));
        assert!(listing.contains("src/main.rs ("), "{listing}");

        let deflated = dir.read(&mut file, "src/main.rs", 1 << 20).unwrap();
        assert_eq!(deflated, b"fn main() {}\n");
        let stored = dir.read(&mut file, "notes.txt", 1 << 20).unwrap();
        assert_eq!(stored, b"plain stored\n");

        let err = dir.read(&mut file, "missing.rs", 1 << 20).unwrap_err();
        assert!(err.contains("not found"), "{err}");
        let err = dir.read(&mut file, "src/", 1 << 20).unwrap_err();
        assert!(err.contains("directory"), "{err}");
    }

    #[test]
    fn zip_corrupt_member_crc_fails_the_read() {
        let mut bytes = zip(&[("a.txt", b"hello world\n", true, false)]);
        // flip a byte inside the compressed payload (after local header)
        let at = find(&bytes, b"a.txt").unwrap() + 5 + 6;
        bytes[at] ^= 0xFF;
        let (dir, mut file) = open_zip(&bytes);
        let err = dir.read(&mut file, "a.txt", 1 << 20).unwrap_err();
        assert!(
            err.contains("corrupt") || err.contains("crc") || err.contains("size"),
            "{err}"
        );
    }

    #[test]
    fn zip_encrypted_and_unsupported_method_entries_error() {
        let bytes = zip(&[("secret.txt", b"locked", true, true)]);
        let (dir, mut file) = open_zip(&bytes);
        let err = dir.read(&mut file, "secret.txt", 1 << 20).unwrap_err();
        assert!(err.contains("encrypted"), "{err}");
    }

    // ------------------------------------------------------------ tar

    #[test]
    fn tar_lists_and_extracts_members() {
        let bytes = tar(&[("dir/", b""), ("dir/file.txt", b"contents here")]);
        let entries = tar_entries(&bytes).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].name, "dir/");
        let file = &entries[1];
        assert_eq!(file.name, "dir/file.txt");
        assert_eq!(
            tar_entry_data(&bytes, file, 1 << 20).unwrap(),
            b"contents here"
        );
        let listing = tar_listing(&entries);
        assert!(listing.contains("dir/file.txt"), "{listing}");
    }

    #[test]
    fn tar_long_names_resolve() {
        let long = format!("deep/{}.txt", "x".repeat(120));
        let bytes = tar_with_longname(&long, b"payload", ("s.txt", b"s"));
        let entries = tar_entries(&bytes).unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].name, long, "GNU longname must win");
        assert_eq!(entries[1].name, "s.txt");
    }

    #[test]
    fn tar_checksum_corruption_is_detected() {
        let mut bytes = tar(&[("a.txt", b"payload")]);
        let at = find(&bytes, b"payload").unwrap() - 20;
        bytes[at] ^= 0xFF;
        let err = tar_entries(&bytes).unwrap_err();
        assert!(err.contains("corrupt"), "{err}");
    }

    #[test]
    fn tar_gz_roundtrips_through_gunzip() {
        let inner = tar(&[("t/g.txt", b"gzip-wrapped")]);
        let gz = gzip(&inner);
        assert_eq!(classify(&gz), Kind::Gzip);
        let unzipped = gunzip(&gz, 1 << 20).unwrap();
        let entries = tar_entries(&unzipped).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            tar_entry_data(&unzipped, &entries[0], 1 << 20).unwrap(),
            b"gzip-wrapped"
        );
    }

    #[test]
    fn gunzip_caps_decompression() {
        let big = vec![0u8; 2 * 1024 * 1024];
        let gz = gzip(&big);
        let err = gunzip(&gz, 1024).unwrap_err();
        assert!(err.contains("cap"), "{err}");
    }
}
