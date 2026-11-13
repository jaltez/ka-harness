//! `ka attach` (roadmap 9.6): observe-only client for a live
//! `ka serve` session. Resolves the session over `GET /sessions`, then
//! streams `GET /sessions/{id}/events` (SSE) into the ka-term observer
//! UI. There is deliberately NO write path here — prompting stays on
//! the server's HTTP API (`POST /sessions/{id}/prompt`), which is what
//! "single writer, server-enforced" means in practice: an attach
//! client cannot prompt, steer, or answer asks.

use std::process::ExitCode;
use std::time::Duration;

use ka_protocol::Event;

/// Run the attach client. `session` is a server-session id prefix
/// (or a title substring); `None` picks the server's newest session.
pub async fn run(
    session: Option<String>,
    addr: &str,
    token: Option<String>,
) -> Result<ExitCode, String> {
    let token = token.or_else(|| {
        std::env::var("KA_SERVE_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let mut req = client
        .get(format!("{base}/sessions"))
        .timeout(Duration::from_secs(5));
    if let Some(token) = &token {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("connect {base}: {e} (is `ka serve --addr {addr}` running?)"))?;
    if !resp.status().is_success() {
        return Err(format!("GET /sessions: {}", resp.status()));
    }
    let rows: Vec<serde_json::Value> = resp
        .json()
        .await
        .map_err(|e| format!("GET /sessions: {e}"))?;
    if rows.is_empty() {
        return Err(format!(
            "no live sessions on {base} — POST /sessions to create one"
        ));
    }
    let id = pick_session(session.as_deref(), &rows)?;

    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(256);
    let stream_client = client.clone();
    let stream_url = format!("{base}/sessions/{id}/events");
    let stream_token = token.clone();
    let stream_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failed = stream_failed.clone();
    tokio::spawn(async move {
        if let Err(note) = stream_events(stream_client, &stream_url, stream_token, tx.clone()).await
        {
            failed.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = tx
                .send(Event::Error {
                    class: ka_protocol::ErrorClass::Protocol,
                    retryable: false,
                    message: format!("attach stream closed: {note}"),
                })
                .await;
        }
    });
    ka_term::observer::run(rx, &id, addr)
        .await
        .map_err(|e| format!("observer: {e}"))?;
    if stream_failed.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve the session reference against the server's listing: an id
/// prefix match first (s1, s12…), then a title substring; `None` picks
/// the highest-numbered (newest) session.
fn pick_session(needle: Option<&str>, rows: &[serde_json::Value]) -> Result<String, String> {
    fn id_of(v: &serde_json::Value) -> &str {
        v.get("id").and_then(|i| i.as_str()).unwrap_or("")
    }
    // s2 vs s10: order by the numeric suffix, not lexicographically
    fn id_rank(v: &serde_json::Value) -> u64 {
        id_of(v).trim_start_matches('s').parse().unwrap_or(0)
    }
    match needle {
        None => {
            let mut best = &rows[0];
            for row in rows {
                if id_rank(row) > id_rank(best) {
                    best = row;
                }
            }
            Ok(id_of(best).to_string())
        }
        Some(n) => {
            // exact id wins even when it is also a prefix of others
            // (`ka attach s1` must work while `s10` exists)
            if let Some(exact) = rows.iter().find(|r| id_of(r) == n) {
                return Ok(id_of(exact).to_string());
            }
            let id_matches: Vec<&serde_json::Value> =
                rows.iter().filter(|r| id_of(r).starts_with(n)).collect();
            if id_matches.len() == 1 {
                return Ok(id_of(id_matches[0]).to_string());
            }
            if id_matches.len() > 1 {
                let ids: Vec<String> = id_matches.iter().map(|r| id_of(r).to_string()).collect();
                return Err(format!("session id '{n}' is ambiguous: {}", ids.join(", ")));
            }
            let title_matches: Vec<&serde_json::Value> = rows
                .iter()
                .filter(|r| {
                    r.get("title")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t.to_lowercase().contains(&n.to_lowercase()))
                })
                .collect();
            match title_matches.len() {
                1 => Ok(id_of(title_matches[0]).to_string()),
                0 => Err(format!("no session matches '{n}' on this server")),
                _ => {
                    let ids: Vec<String> =
                        title_matches.iter().map(|r| id_of(r).to_string()).collect();
                    Err(format!("session '{n}' is ambiguous: {}", ids.join(", ")))
                }
            }
        }
    }
}

/// Stream one SSE endpoint into `tx`, reconnecting with the last seen
/// SSE id (`Last-Event-ID`) whenever the stream drops: the server
/// replays the gap from its ring, so events are not lost. `Ok(())`
/// means the observer went away (clean shutdown); `Err` means the
/// stream could not be re-established after bounded retries.
const MAX_RECONNECTS: u32 = 5;

async fn stream_events(
    client: reqwest::Client,
    url: &str,
    token: Option<String>,
    tx: tokio::sync::mpsc::Sender<Event>,
) -> Result<(), String> {
    let mut last_id: Option<String> = None;
    let mut note = String::new();
    for attempt in 0..=MAX_RECONNECTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        match stream_once(&client, url, &token, &tx, &mut last_id).await {
            Ok(()) => return Ok(()), // observer gone: clean shutdown
            Err(n) => note = n,
        }
    }
    Err(note)
}

/// One connection attempt. `Err` covers both transport failure and a
/// server-side close; both are reconnectable via `Last-Event-ID`.
async fn stream_once(
    client: &reqwest::Client,
    url: &str,
    token: &Option<String>,
    tx: &tokio::sync::mpsc::Sender<Event>,
    last_id: &mut Option<String>,
) -> Result<(), String> {
    let mut req = client.get(url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    if let Some(id) = &*last_id {
        req = req.header("Last-Event-ID", id);
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| format!("{e}"))?
        .error_for_status()
        .map_err(|e| format!("{e}"))?;
    // Raw bytes: frames are converted to UTF-8 only once complete, so
    // multi-byte chars split across chunk boundaries survive.
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| format!("{e}"))?
            .ok_or("stream closed")?;
        buffer.extend_from_slice(&chunk);
        while let Some(frame) = extract_frame(&mut buffer) {
            for evt in parse_frame(&frame, last_id) {
                if tx.send(evt).await.is_err() {
                    return Ok(()); // observer gone
                }
            }
        }
    }
}

/// Pop one complete SSE frame (blank-line terminated) off `buffer`,
/// leaving any incomplete tail buffered. The server terminates frames
/// with `\r\n\r\n` (which contains no `\n\n`, hence the explicit
/// search); plain `\n\n` separators are tolerated. The earliest
/// boundary present wins.
fn extract_frame(buffer: &mut Vec<u8>) -> Option<String> {
    let crlf = buffer.windows(4).position(|w| w == b"\r\n\r\n");
    let lf = buffer.windows(2).position(|w| w == b"\n\n");
    let (idx, sep) = match (crlf, lf) {
        (Some(a), Some(b)) if a < b => (a, 4),
        (_, Some(b)) => (b, 2),
        (Some(a), None) => (a, 4),
        (None, None) => return None,
    };
    let frame = String::from_utf8_lossy(&buffer[..idx]).into_owned();
    buffer.drain(..idx + sep);
    Some(frame)
}

/// Parse the `id:`/`data:` lines of one SSE frame into events.
/// Malformed JSON surfaces as `Event::Error` rather than being silently
/// dropped. A data line that IS valid JSON but carries a `type` this
/// client does not know (a newer server's event) is skipped: the tagged
/// `Event` enum has no catch-all, so deserializing would fail — the
/// failure mode is a newer-server/older-client pair, not corruption.
fn parse_frame(frame: &str, last_id: &mut Option<String>) -> Vec<Event> {
    let mut out = Vec::new();
    for line in frame.lines() {
        if let Some(id) = line.strip_prefix("id:") {
            let id = id.trim();
            if !id.is_empty() {
                *last_id = Some(id.to_string());
            }
        }
        let payload = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"));
        if let Some(payload) = payload {
            let payload = payload.trim();
            // malformed JSON is a visible error row; valid JSON whose
            // `type` this build does not know is a newer peer — skip it
            // (serde reports the tag before the rest of the map, so the
            // typed error alone cannot tell the two apart)
            let json_ok = serde_json::from_str::<serde_json::Value>(payload).is_ok();
            match ka_protocol::from_line::<Event>(payload) {
                Ok(evt) => out.push(evt),
                Err(_) if json_ok => {}
                Err(e) => out.push(Event::Error {
                    class: ka_protocol::ErrorClass::Protocol,
                    retryable: false,
                    message: format!("attach: unparseable SSE data line ({e}): {payload}"),
                }),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use serde_json::json;

    #[test]
    fn session_picking_prefers_exact_then_prefix_then_title_then_newest() {
        let rows = vec![
            json!({"id": "s1", "title": "fix the parser"}),
            json!({"id": "s2", "title": "write docs"}),
            json!({"id": "s10", "title": "refactor engine"}),
        ];
        // None → newest (highest id)
        assert_eq!(super::pick_session(None, &rows).unwrap(), "s10");
        // id prefix, unique
        assert_eq!(super::pick_session(Some("s2"), &rows).unwrap(), "s2");
        // exact id wins even when it is also a prefix of s10
        assert_eq!(super::pick_session(Some("s1"), &rows).unwrap(), "s1");
        // id prefix ambiguity is an error ("s" matches everything)
        assert!(super::pick_session(Some("s"), &rows).is_err());
        // title substring fallback
        assert_eq!(super::pick_session(Some("docs"), &rows).unwrap(), "s2");
        assert!(super::pick_session(Some("nope"), &rows).is_err());
    }

    #[test]
    fn crlf_frames_extract_and_partial_stays_buffered() {
        let mut buffer = b"id: 7\r\ndata: {\"type\":\"presence\",\"busy\":false,\"attached\":1}\r\n\r\nid: 8\r\nda"
            .to_vec();
        let frame = super::extract_frame(&mut buffer).unwrap();
        // partial trailing frame stays buffered, no second frame yet
        assert!(super::extract_frame(&mut buffer).is_none());
        assert_eq!(buffer, b"id: 8\r\nda");
        // the complete frame parses: id tracked, event extracted
        let mut last_id = None;
        let events = super::parse_frame(&frame, &mut last_id);
        assert_eq!(last_id.as_deref(), Some("7"));
        assert!(matches!(
            events.as_slice(),
            [ka_protocol::Event::Presence {
                busy: false,
                attached: 1
            }]
        ));
    }

    #[test]
    fn plain_lf_frames_and_unparseable_lines_surface() {
        let mut buffer = b"data: not json at all\n\n".to_vec();
        let frame = super::extract_frame(&mut buffer).unwrap();
        assert!(buffer.is_empty());
        let mut last_id = None;
        let events = super::parse_frame(&frame, &mut last_id);
        // garbage is not silently dropped: it becomes an Event::Error
        assert!(matches!(
            events.as_slice(),
            [ka_protocol::Event::Error { .. }]
        ));
    }

    #[test]
    fn unknown_event_type_frame_is_skipped_not_errored() {
        // a newer server emits an event type this client's Event enum
        // does not know: valid JSON, unknown `type` → skip the frame
        // entirely (no Event::Error row), unlike malformed JSON.
        let mut buffer = b"data: {\"type\":\"hologram\",\"holo\":true}\n\n".to_vec();
        let frame = super::extract_frame(&mut buffer).unwrap();
        let mut last_id = None;
        let events = super::parse_frame(&frame, &mut last_id);
        assert!(events.is_empty());

        // same frame family: malformed JSON still surfaces as an error
        let mut buffer = b"data: {\"type\":\"hologram\", nope}\n\n".to_vec();
        let frame = super::extract_frame(&mut buffer).unwrap();
        let mut last_id = None;
        let events = super::parse_frame(&frame, &mut last_id);
        assert!(matches!(
            events.as_slice(),
            [ka_protocol::Event::Error { .. }]
        ));
    }
}
