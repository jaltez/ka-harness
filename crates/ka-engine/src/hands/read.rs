//! The read hand: line-ranged file reads with caps and ledger minting.
//! Image files (png/jpg/webp/gif, detected by magic bytes) come back as
//! a placeholder plus an `ImagePart` for vision models.
use std::future::Future;
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::pin::Pin;

use serde_json::{Value, json};

use super::{Hand, HandContext, HandDef, ToolOutput};

/// Maximum lines returned in one read.
pub const MAX_LINES: usize = 2_000;
/// Maximum bytes returned in one read.
pub const MAX_BYTES: usize = 256_000;

/// The read tool.
pub struct ReadHand;

impl Hand for ReadHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "read".to_string(),
            description: "Read a file. Returns numbered lines. Use offset/limit for ranges. \
                Directories return a shallow listing. PDFs return extracted text; zip/jar/apk/whl \
                and tar/tar.gz archives list their members, and archive_entry reads one member."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path (relative to cwd or absolute)" },
                    "offset": { "type": "integer", "description": "1-based first line" },
                    "limit": { "type": "integer", "description": "Max lines to return" },
                    "archive_entry": { "type": "string", "description": "Member path inside an \
                        archive (zip/jar/apk/whl/tar/tar.gz) to read instead of listing it" }
                },
                "required": ["path"]
            }),
            clearance: super::Clearance::Read,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(path_str) = args.get("path").and_then(Value::as_str) else {
                return ToolOutput::err("read: missing required 'path'");
            };
            let path = resolve(ctx, path_str);
            let offset = args
                .get("offset")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .max(1) as usize;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|l| (l as usize).min(MAX_LINES))
                .unwrap_or(MAX_LINES);

            // File reads are blocking I/O (image payloads reach MBs): run
            // them on the blocking pool so the single-threaded async
            // runtime keeps pumping events during the read.
            let entry = args
                .get("archive_entry")
                .and_then(Value::as_str)
                .map(str::to_string);
            let ledger = ctx.ledger.clone();
            let spill = ctx.spill.clone();
            let cap_mb = ctx.max_image_mb;
            let joined = tokio::task::spawn_blocking(move || {
                read_blocking(
                    &path,
                    offset,
                    limit,
                    cap_mb,
                    &ledger,
                    &spill,
                    entry.as_deref(),
                )
            })
            .await;
            match joined {
                Ok(output) => output,
                Err(e) => ToolOutput::err(format!("read: task failed: {e}")),
            }
        })
    }
}

/// Extracted text over this size parks on the spill store; the model
/// gets a capped excerpt plus the `spill://` pointer.
const EXTRACT_SPILL_AT: usize = 32 * 1024;
/// Raw PDF files over this are refused (extraction reads them whole).
const PDF_RAW_CAP: u64 = 64 * 1024 * 1024;
/// Raw tar files over this are refused (tar parsing reads them whole).
const TAR_RAW_CAP: u64 = 256 * 1024 * 1024;
/// Decompressed gzip/tar content cap (zip-bomb guard).
const ARCHIVE_DECOMP_CAP: usize = 64 * 1024 * 1024;
/// Per-archive-member extraction cap.
const MEMBER_CAP: usize = 16 * 1024 * 1024;
/// Maximum bytes buffered for a single line (prevents OOM on newline-free files).
const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;

/// Read a single line up to `cap` bytes; any remainder past `cap` up to
/// the next newline is discarded without allocating.
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<usize> {
    buf.clear();
    let n = reader.take(cap as u64).read_until(b'\n', buf)?;
    if n == cap && !buf.ends_with(b"\n") {
        loop {
            let avail = reader.fill_buf()?;
            if avail.is_empty() {
                break;
            }
            if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                reader.consume(pos + 1);
                break;
            }
            let len = avail.len();
            reader.consume(len);
        }
    }
    Ok(n)
}

/// Blocking worker: the whole read — metadata, listing, minting, image
/// sniffing/base64, rich-format extraction, line windowing.
fn read_blocking(
    path: &std::path::Path,
    offset: usize,
    limit: usize,
    cap_mb: u32,
    ledger: &std::sync::Arc<parking_lot::Mutex<crate::hands::Ledger>>,
    spill: &std::sync::Arc<crate::hands::Spill>,
    archive_entry: Option<&str>,
) -> ToolOutput {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
    };
    if meta.is_dir() {
        return list_dir(path);
    }
    if !meta.is_file() {
        return ToolOutput::err(format!(
            "read {}: refusing non-regular file (FIFO, socket, or device)",
            path.display()
        ));
    }

    // rich formats first: magic-byte detection on the file head
    let mut head = [0u8; 262];
    let head_len = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    let kind = crate::readers::classify(&head[..head_len]);
    if archive_entry.is_some() && matches!(kind, crate::readers::Kind::Other) {
        return ToolOutput::err(format!(
            "read {}: archive_entry was given, but the file has no zip/tar/gzip \
             signature — it is not an archive",
            path.display()
        ));
    }
    match kind {
        crate::readers::Kind::Pdf => return read_pdf(path, meta, offset, limit, ledger, spill),
        crate::readers::Kind::Zip => return read_zip(path, offset, limit, archive_entry, spill),
        crate::readers::Kind::Gzip | crate::readers::Kind::Tar => {
            return read_tar(path, kind, meta, offset, limit, archive_entry, spill);
        }
        crate::readers::Kind::Other => {}
    }

    ledger.lock().mint(path, &meta);

    // image files: sniff by magic bytes, cap size, return a
    // placeholder plus the base64 payload for the vision wire
    if let Some(media_type) = sniff_image_type(path) {
        if cap_mb > 0 && meta.len() > cap_mb as u64 * 1024 * 1024 {
            return ToolOutput::err(format!(
                "read {}: image is {:.1} MB, over the {} MB cap \
                 ([tools.read] max_image_mb)",
                path.display(),
                meta.len() as f64 / (1024.0 * 1024.0),
                cap_mb
            ));
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
        };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("image");
        let kb = bytes.len().div_ceil(1024);
        return ToolOutput {
            content: format!("[image {name} {kb}KB]\n"),
            is_error: false,
            spill: None,
            images: vec![ka_protocol::ImagePart {
                data: b64_encode(&bytes),
                media_type: media_type.to_string(),
            }],
        };
    }

    // stream the file line by line: never materialize the whole text,
    // keep only the requested window (lossy per line — one bad byte
    // must not fail the whole read)
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
    };
    let mut reader = std::io::BufReader::new(file);
    let start = offset.saturating_sub(1);
    let mut total = 0usize;
    let mut window: Vec<String> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
        }
        if total >= start && window.len() < limit {
            let mut line = String::from_utf8_lossy(&buf).into_owned();
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
            }
            window.push(line);
        }
        total += 1;
    }
    ToolOutput::ok(render_window(start, &window, total))
}

/// Numbered-line rendering shared by the streaming path and the
/// extraction paths: `N\tline` rows under the byte cap, plus a
/// remainder trailer when more lines exist.
fn render_window(start: usize, window: &[String], total: usize) -> String {
    let mut out = String::new();
    let mut bytes = 0usize;
    for (i, line) in window.iter().enumerate() {
        let numbered = format!("{}\t{}\n", start + i + 1, line);
        bytes += numbered.len();
        if bytes > MAX_BYTES {
            out.push_str("[...byte cap reached; narrow with offset/limit]\n");
            break;
        }
        out.push_str(&numbered);
    }
    let end = start + window.len();
    if end < total {
        out.push_str(&format!(
            "[...{} of {} lines shown; lines {}-{} remain]\n",
            end - start,
            total,
            end + 1,
            total
        ));
    }
    if out.is_empty() {
        out.push_str("(empty file)\n");
    }
    out
}

/// Render extracted (already in-memory) text with the same numbered
/// window the streaming path uses; text over [`EXTRACT_SPILL_AT`] parks
/// on the spill store and the excerpt points at it.
fn render_extracted(
    text: String,
    label: &str,
    offset: usize,
    limit: usize,
    spill: &crate::hands::Spill,
) -> ToolOutput {
    if text.trim().is_empty() {
        return ToolOutput::ok(format!("({label} contains no extractable text)\n"));
    }
    if text.len() > EXTRACT_SPILL_AT {
        let kb = text.len().div_ceil(1024);
        let pointer = spill.park(&text).ok();
        let mut excerpt: String = text.chars().take(20_000).collect();
        excerpt.push('\n');
        let mut out = render_window(
            offset.saturating_sub(1),
            &excerpt.lines().map(str::to_string).collect::<Vec<_>>(),
            excerpt.lines().count(),
        );
        match pointer {
            Some(ptr) => {
                out.push_str(&format!(
                    "[{label} is {kb} KB — full text parked at {ptr}]\n"
                ));
                let mut tool = ToolOutput::ok(out);
                tool.spill = Some(ptr);
                return tool;
            }
            None => out.push_str(&format!(
                "[{label} is {kb} KB; the spill store rejected the full text]"
            )),
        }
        return ToolOutput::ok(out);
    }
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let start = offset.saturating_sub(1);
    let window: Vec<String> = lines.iter().skip(start).take(limit).cloned().collect();
    ToolOutput::ok(render_window(start, &window, lines.len()))
}

/// PDF text extraction: whole file (size-capped), page order from the
/// page tree. PDFs mint into the ledger like any file read.
fn read_pdf(
    path: &std::path::Path,
    meta: std::fs::Metadata,
    offset: usize,
    limit: usize,
    ledger: &std::sync::Arc<parking_lot::Mutex<crate::hands::Ledger>>,
    spill: &std::sync::Arc<crate::hands::Spill>,
) -> ToolOutput {
    if meta.len() > PDF_RAW_CAP {
        return ToolOutput::err(format!(
            "read {}: the PDF is {:.1} MB, over the {} MB extraction cap",
            path.display(),
            meta.len() as f64 / (1024.0 * 1024.0),
            PDF_RAW_CAP / (1024 * 1024)
        ));
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
    };
    match crate::readers::pdf_text(&bytes) {
        Ok(text) => {
            ledger.lock().mint(path, &meta);
            render_extracted(text, "PDF text", offset, limit, spill)
        }
        Err(e) => ToolOutput::err(format!("read {}: {e}", path.display())),
    }
}

/// ZIP family: no `archive_entry` → member listing; with one → that
/// member's text. Archives are NOT minted into the ledger (they are
/// not edit targets).
fn read_zip(
    path: &std::path::Path,
    offset: usize,
    limit: usize,
    entry: Option<&str>,
    spill: &std::sync::Arc<crate::hands::Spill>,
) -> ToolOutput {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
    };
    let dir = match crate::readers::ZipDir::open(&mut file) {
        Ok(d) => d,
        Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
    };
    let Some(entry) = entry else {
        return ToolOutput::ok(dir.listing());
    };
    match dir.read(&mut file, entry, MEMBER_CAP) {
        Ok(bytes) => {
            if bytes.len() > MEMBER_CAP || bytes.contains(&0) {
                // binary member (class files, images, native libs):
                // lossy decoding would just render garbage
                return ToolOutput::ok(format!(
                    "[archive entry '{entry}' is binary — {} bytes; text \
                     extraction not supported]\n",
                    human_bytes(bytes.len())
                ));
            }
            render_extracted(
                String::from_utf8_lossy(&bytes).into_owned(),
                &format!("archive entry '{entry}'"),
                offset,
                limit,
                spill,
            )
        }
        Err(e) => ToolOutput::err(format!("read {}: {e}", path.display())),
    }
}

/// tar / tar.gz: listing or member read, like zip. A gzip that is not
/// a tar inside (a plain `.gz` file) renders as text.
fn read_tar(
    path: &std::path::Path,
    kind: crate::readers::Kind,
    meta: std::fs::Metadata,
    offset: usize,
    limit: usize,
    entry: Option<&str>,
    spill: &std::sync::Arc<crate::hands::Spill>,
) -> ToolOutput {
    let raw_cap = match kind {
        crate::readers::Kind::Gzip => ARCHIVE_DECOMP_CAP as u64,
        _ => TAR_RAW_CAP,
    };
    if meta.len() > raw_cap {
        return ToolOutput::err(format!(
            "read {}: the archive is {:.1} MB, over the {} MB extraction cap",
            path.display(),
            meta.len() as f64 / (1024.0 * 1024.0),
            raw_cap / (1024 * 1024)
        ));
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
    };
    let payload = if kind == crate::readers::Kind::Gzip {
        match crate::readers::gunzip(&bytes, ARCHIVE_DECOMP_CAP) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
        }
    } else {
        bytes
    };
    // tar inside, or a plain gzip'd file?
    let looks_tar = payload.len() >= 262 && &payload[257..262] == b"ustar";
    if !looks_tar {
        let text = String::from_utf8_lossy(&payload).into_owned();
        return render_extracted(text, "decompressed gzip content", offset, limit, spill);
    }
    let entries = match crate::readers::tar_entries(&payload) {
        Ok(e) => e,
        Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
    };
    let Some(entry) = entry else {
        return ToolOutput::ok(crate::readers::tar_listing(&entries));
    };
    let Some(te) = entries.iter().find(|e| e.name == entry) else {
        return ToolOutput::err(format!(
            "read {}: entry '{entry}' not found in the archive — read it without \
             archive_entry for the member listing",
            path.display()
        ));
    };
    if te.is_dir {
        return ToolOutput::err(format!(
            "read {}: archive entry '{entry}' is a directory",
            path.display()
        ));
    }
    match crate::readers::tar_entry_data(&payload, te, MEMBER_CAP) {
        Ok(data) => {
            if data.contains(&0) {
                return ToolOutput::ok(format!(
                    "[archive entry '{entry}' is binary — {} bytes; text \
                     extraction not supported]\n",
                    human_bytes(data.len())
                ));
            }
            render_extracted(
                String::from_utf8_lossy(data).into_owned(),
                &format!("archive entry '{entry}'"),
                offset,
                limit,
                spill,
            )
        }
        Err(e) => ToolOutput::err(format!("read {}: {e}", path.display())),
    }
}

fn human_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else {
        format!("{:.1} KB", n as f64 / 1024.0)
    }
}

fn list_dir(path: &std::path::Path) -> ToolOutput {
    let Ok(entries) = std::fs::read_dir(path) else {
        return ToolOutput::err(format!("read {}: cannot list", path.display()));
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .take(500)
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    names.sort();
    if names.is_empty() {
        return ToolOutput::ok("(empty directory)");
    }
    ToolOutput::ok(names.join("\n"))
}

/// Detect an image file by its leading magic bytes. Returns the IANA
/// media type.
fn sniff_image_type(path: &std::path::Path) -> Option<&'static str> {
    let mut head = [0u8; 12];
    let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    let h = &head[..n];
    if h.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if h.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if h.starts_with(b"GIF87a") || h.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if h.len() >= 12 && h.starts_with(b"RIFF") && &h[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Minimal standard base64 encoder (keeps ka dependency-free).
fn b64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub(crate) fn resolve(ctx: &HandContext, p: &str) -> PathBuf {
    let expanded = if let Some(rest) = p.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|_| PathBuf::from(p))
    } else {
        PathBuf::from(p)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        ctx.cwd.join(expanded)
    }
}

/// Resolve `raw` against `ctx.cwd` (expanding `~/`) and lexically
/// normalize `..`/`.` while canonicalizing the deepest existing
/// ancestor to defeat symlink escapes.
#[allow(dead_code)]
pub(crate) fn resolve_normalized(ctx: &HandContext, raw: &str) -> PathBuf {
    super::protected::canonicalize_deepest(&resolve(ctx, raw))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;
    use crate::hands::Ledger;

    fn ctx_for(dir: &std::path::Path) -> HandContext {
        HandContext {
            cwd: dir.to_path_buf(),
            ledger: Arc::new(Mutex::new(Ledger::default())),
            spill: Arc::new(super::super::Spill::new()),
            snapshots: Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
            shell_env: Default::default(),
        }
    }

    #[tokio::test]
    async fn reads_numbered_lines_with_range() {
        let dir = std::env::temp_dir().join(format!("ka-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand
            .execute(&json!({"path": "f.txt", "offset": 2, "limit": 2}), &ctx)
            .await;
        assert!(!out.is_error);
        assert_eq!(
            out.content,
            "2\tl2\n3\tl3\n[...2 of 5 lines shown; lines 4-5 remain]\n"
        );
        assert!(
            ctx.ledger.lock().verify(&dir.join("f.txt")).is_ok(),
            "read must mint ledger"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_huge_file_returns_window() {
        let dir = std::env::temp_dir().join(format!("ka-read-huge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.txt");
        // ~200k lines written incrementally — the reader must never
        // materialize the whole file, only the requested window
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path).unwrap();
            for i in 1..=200_000u32 {
                writeln!(f, "line-{i}").unwrap();
            }
        }
        let ctx = ctx_for(&dir);
        let out = ReadHand
            .execute(
                &json!({"path": "huge.txt", "offset": 199_995, "limit": 3}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            out.content,
            "199995\tline-199995\n199996\tline-199996\n199997\tline-199997\n\
             [...3 of 200000 lines shown; lines 199998-200000 remain]\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn invalid_utf8_lines_decode_lossily() {
        // deliberate behavior change vs the old read_to_string (which
        // hard-failed the whole read): one bad byte must not lose the
        // good lines around it — lines decode per-line with U+FFFD
        let dir = std::env::temp_dir().join(format!("ka-read-utf8-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mixed.bin");
        std::fs::write(&path, b"ok line\n\xff\xfe bad bytes\nend line\n").unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand.execute(&json!({"path": "mixed.bin"}), &ctx).await;
        assert!(
            !out.is_error,
            "lossy decode must not error: {}",
            out.content
        );
        assert!(out.content.contains("ok line"), "{}", out.content);
        assert!(out.content.contains("end line"), "{}", out.content);
        assert!(
            out.content.contains('\u{FFFD}'),
            "replacement char marks the bad line: {}",
            out.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let ctx = ctx_for(std::path::Path::new("/tmp"));
        let out = ReadHand
            .execute(&json!({"path": "definitely-missing-ka"}), &ctx)
            .await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn lists_directories() {
        let dir = std::env::temp_dir().join(format!("ka-read-dir-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand.execute(&json!({"path": "."}), &ctx).await;
        assert!(!out.is_error);
        assert!(out.content.contains("a.txt"));
        assert!(out.content.contains("sub/"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_returns_image_part_for_png() {
        let dir = std::env::temp_dir().join(format!("ka-read-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 1x1 transparent PNG
        let png: &[u8] = &[
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ];
        std::fs::write(dir.join("pic.png"), png).unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand
            .execute(&serde_json::json!({"path": "pic.png"}), &ctx)
            .await;
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        assert_eq!(out.images[0].data, b64_encode(png));
        assert!(out.content.contains("[image pic.png"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_image_respects_max_image_mb() {
        let dir = std::env::temp_dir().join(format!("ka-read-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let jpeg: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        std::fs::write(dir.join("big.jpg"), jpeg).unwrap();
        let mut ctx = ctx_for(&dir);
        ctx.max_image_mb = 0; // 0 = unlimited
        let out = ReadHand
            .execute(&serde_json::json!({"path": "big.jpg"}), &ctx)
            .await;
        assert!(!out.is_error);
        assert_eq!(out.images[0].media_type, "image/jpeg");

        // 1.1 MB file against a 1 MB cap
        let mut big: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        big.resize(1100 * 1024, 0);
        std::fs::write(dir.join("big.jpg"), &big).unwrap();
        let mut ctx = ctx_for(&dir);
        ctx.max_image_mb = 1;
        let out = ReadHand
            .execute(&serde_json::json!({"path": "big.jpg"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("max_image_mb"), "{}", out.content);
        assert!(out.images.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn b64_encode_matches_known_vectors() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(b64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn sniff_rejects_text_files() {
        let dir = std::env::temp_dir().join(format!("ka-read-sniff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plain.txt");
        std::fs::write(&p, "just text\n").unwrap();
        assert_eq!(sniff_image_type(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------- rich readers (lote 8)

    use crate::readers::fixtures as fx;

    #[tokio::test]
    async fn read_pdf_extracts_text_in_page_order_and_mints() {
        let dir = std::env::temp_dir().join(format!("ka-read-pdf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bytes = fx::pdf(
            &[
                "BT (alpha page) Tj ET",
                "BT (beta page) Tj ET",
                "BT (gamma page) Tj ET",
            ],
            true,
        );
        std::fs::write(dir.join("doc.pdf"), &bytes).unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand.execute(&json!({"path": "doc.pdf"}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        // page order: alpha < beta < gamma, pages separated by form feed
        let alpha = out.content.find("alpha page").unwrap();
        let beta = out.content.find("beta page").unwrap();
        let gamma = out.content.find("gamma page").unwrap();
        assert!(alpha < beta && beta < gamma, "{}", out.content);
        assert!(
            ctx.ledger.lock().is_tracked(&dir.join("doc.pdf")),
            "PDF reads mint into the ledger like file reads"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_pdf_spills_text_over_the_32kib_gate() {
        let dir = std::env::temp_dir().join(format!("ka-read-pdfbig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // ~3000 lines ≈ 40 KB of extracted text — over the spill gate
        let mut body = String::from("BT\n");
        for i in 0..3000 {
            body.push_str(&format!("(filler line {i:05}) Tj T*\n"));
        }
        body.push_str("ET");
        let bytes = fx::pdf(&[&body], true);
        std::fs::write(dir.join("big.pdf"), &bytes).unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand.execute(&json!({"path": "big.pdf"}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.spill.is_some(),
            "40 KB of text must spill: {:?}",
            out.spill
        );
        assert!(out.content.contains("spill://"), "{}", out.content);
        assert!(out.content.contains("KB"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_encrypted_pdf_is_a_clear_error() {
        let dir = std::env::temp_dir().join(format!("ka-read-pdfenc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bytes = fx::encrypted_pdf(&["BT (locked) Tj ET"]);
        std::fs::write(dir.join("locked.pdf"), &bytes).unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand.execute(&json!({"path": "locked.pdf"}), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("encrypted"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_zip_lists_members_and_reads_entries_without_minting() {
        let dir = std::env::temp_dir().join(format!("ka-read-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bytes = fx::zip(&[
            ("src/", b"", false, false),
            (
                "src/main.rs",
                b"fn main() {\n    println!(\"hi\");\n}\n",
                true,
                false,
            ),
            ("README", b"stored\n", false, false),
        ]);
        std::fs::write(dir.join("app.jar"), &bytes).unwrap();
        let ctx = ctx_for(&dir);

        // no archive_entry: the member listing
        let out = ReadHand.execute(&json!({"path": "app.jar"}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("(3 entries)"), "{}", out.content);
        assert!(out.content.contains("src/main.rs"), "{}", out.content);

        // archive_entry: the member's text
        let out = ReadHand
            .execute(
                &json!({"path": "app.jar", "archive_entry": "src/main.rs"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("println!"), "{}", out.content);

        // archives never mint into the ledger (not edit targets)
        assert!(
            !ctx.ledger.lock().is_tracked(&dir.join("app.jar")),
            "archive reads must not mint ledger stamps"
        );

        // a missing member names the listing escape hatch
        let out = ReadHand
            .execute(
                &json!({"path": "app.jar", "archive_entry": "nope.txt"}),
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("not found"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_tar_gz_reads_members_through_gzip() {
        let dir = std::env::temp_dir().join(format!("ka-read-tgz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let inner = fx::tar(&[("pkg/crate.txt", b"tar gz member\n")]);
        std::fs::write(dir.join("bundle.tar.gz"), fx::gzip(&inner)).unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand
            .execute(
                &json!({"path": "bundle.tar.gz", "archive_entry": "pkg/crate.txt"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("tar gz member"), "{}", out.content);

        // listing works too
        let out = ReadHand
            .execute(&json!({"path": "bundle.tar.gz"}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("pkg/crate.txt"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn archive_entry_on_a_non_archive_is_an_error() {
        let dir = std::env::temp_dir().join(format!("ka-read-noarch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plain.txt"), "not an archive\n").unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand
            .execute(
                &json!({"path": "plain.txt", "archive_entry": "inside.txt"}),
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("not an archive"), "{}", out.content);
        // and the plain read of the same file still works + mints
        let out = ReadHand.execute(&json!({"path": "plain.txt"}), &ctx).await;
        assert!(!out.is_error);
        assert!(ctx.ledger.lock().is_tracked(&dir.join("plain.txt")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
