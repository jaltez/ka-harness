//! Shared HTTP plumbing: request building, status classification, retries
//! with backoff. Retrying happens at status level only — once deltas are on
//! the wire, mid-stream failures surface as retryable `Failed` events
//! without an automatic re-request (partial output must not be duplicated).

use std::time::Duration;

use ka_protocol::ErrorClass;

/// A wire-level failure with classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    /// Classification for the engine.
    pub class: ErrorClass,
    /// Whether a retry could help.
    pub retryable: bool,
    /// Human-readable detail.
    pub message: String,
}

impl WireError {
    /// Build a retryable network error.
    pub fn network(message: impl Into<String>) -> Self {
        Self {
            class: ErrorClass::Network,
            retryable: true,
            message: message.into(),
        }
    }
}

enum Attempt {
    Ok(reqwest::Response),
    Err(reqwest::Error),
    Timeout,
}

/// Classify an HTTP error status (+ response body snippet).
pub fn classify_status(status: reqwest::StatusCode, body: &str) -> WireError {
    let snippet: String = body.chars().take(400).collect();
    let message = if snippet.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        format!("HTTP {}: {}", status.as_u16(), snippet)
    };
    match status.as_u16() {
        401 | 403 => WireError {
            class: ErrorClass::Auth,
            retryable: false,
            message,
        },
        408 => WireError {
            class: ErrorClass::Network,
            retryable: true,
            message,
        },
        429 => WireError {
            class: ErrorClass::RateLimit,
            retryable: true,
            message,
        },
        400 => {
            let lower = message.to_lowercase();
            let overflowish = lower.contains("context length")
                || lower.contains("context window")
                || lower.contains("too long")
                || lower.contains("prompt is too")
                || lower.contains("maximum context");
            WireError {
                class: if overflowish {
                    ErrorClass::Overflow
                } else {
                    ErrorClass::Protocol
                },
                retryable: false,
                message,
            }
        }
        500..=599 => WireError {
            class: ErrorClass::Network,
            retryable: true,
            message,
        },
        _ => WireError {
            class: ErrorClass::Protocol,
            retryable: false,
            message,
        },
    }
}

/// POST a JSON body and return the streaming response, retrying
/// status-level failures with jittered exponential backoff (and
/// `Retry-After` when present). `first_byte_timeout_ms` of 0 means
/// unbounded (local prefill).
pub async fn post_sse(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: String,
    first_byte_timeout_ms: u64,
    attempts: u32,
) -> Result<reqwest::Response, WireError> {
    let header_map = clone_headers(headers)?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let request = client
            .post(url)
            .headers(header_map.clone())
            .body(body.clone());
        let sent = if first_byte_timeout_ms == 0 {
            match request.send().await {
                Ok(r) => Attempt::Ok(r),
                Err(e) => Attempt::Err(e),
            }
        } else {
            match tokio::time::timeout(Duration::from_millis(first_byte_timeout_ms), request.send())
                .await
            {
                Ok(Ok(r)) => Attempt::Ok(r),
                Ok(Err(e)) => Attempt::Err(e),
                Err(_) => Attempt::Timeout,
            }
        };
        match sent {
            Attempt::Ok(resp) if resp.status().is_success() => return Ok(resp),
            Attempt::Ok(resp) => {
                let status = resp.status();
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                let body = resp.text().await.unwrap_or_default();
                let err = classify_status(status, &body);
                if !err.retryable || attempt >= attempts {
                    return Err(err);
                }
                tokio::time::sleep(retry_delay(attempt, retry_after)).await;
            }
            Attempt::Err(e) => {
                let err = WireError::network(e.to_string());
                if attempt >= attempts {
                    return Err(err);
                }
                tokio::time::sleep(retry_delay(attempt, None)).await;
            }
            Attempt::Timeout => {
                let err = WireError::network(format!(
                    "no first byte within {first_byte_timeout_ms}ms (connect or prefill stalled)"
                ));
                if attempt >= attempts {
                    return Err(err);
                }
                tokio::time::sleep(retry_delay(attempt, None)).await;
            }
        }
    }
}

fn clone_headers(headers: &[(String, String)]) -> Result<reqwest::header::HeaderMap, WireError> {
    let mut map = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        let name = match reqwest::header::HeaderName::try_from(k.as_str()) {
            Ok(n) => n,
            Err(_) => return Err(unencodable_header(k, v, false)),
        };
        let value = match reqwest::header::HeaderValue::try_from(v.as_str()) {
            Ok(val) => val,
            Err(_) => return Err(unencodable_header(k, v, true)),
        };
        map.insert(name, value);
    }
    Ok(map)
}

/// A header name or value containing characters HTTP headers cannot
/// carry (control bytes, DEL). Multibyte UTF-8 is legal obs-text and
/// still sends. Never silently dropped: an omitted auth header would
/// surface as an undiagnosable 401.
fn unencodable_header(name: &str, value: &str, value_is_culprit: bool) -> WireError {
    // control bytes must not ride the diagnostic (a BEL beeps, a
    // newline breaks the message across lines) — show U+FFFD instead
    let sanitize = |s: &str| {
        s.chars()
            .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
            .take(16)
            .collect::<String>()
    };
    let (kind, shown) = if value_is_culprit {
        ("value", sanitize(value))
    } else {
        ("name", sanitize(name))
    };
    WireError {
        class: ErrorClass::Protocol,
        retryable: false,
        message: format!(
            "header `{name}` cannot be sent: the {kind} `{shown}` contains control \
             characters not representable in an HTTP header (check the token for \
             stray newlines or non-printable characters)"
        ),
    }
}

fn retry_delay(attempt: u32, retry_after: Option<u64>) -> Duration {
    if let Some(secs) = retry_after {
        return Duration::from_secs(secs.min(60));
    }
    let base_ms = 500u64.saturating_mul(1 << (attempt - 1).min(5));
    let jitter = pseudo_jitter_ms(250);
    Duration::from_millis(base_ms.min(8_000) + jitter)
}

/// Deterministic-ish jitter without a rand dependency.
fn pseudo_jitter_ms(bound: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    if bound == 0 { 0 } else { nanos % bound }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn status_classification() {
        assert_eq!(
            classify_status(reqwest::StatusCode::UNAUTHORIZED, "").class,
            ErrorClass::Auth
        );
        let rl = classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS, "");
        assert_eq!((rl.class, rl.retryable), (ErrorClass::RateLimit, true));
        let overflow = classify_status(
            reqwest::StatusCode::BAD_REQUEST,
            "prompt is too long: 200000 tokens > 128000 maximum",
        );
        assert_eq!(overflow.class, ErrorClass::Overflow);
        assert!(!overflow.retryable);
        let bad = classify_status(reqwest::StatusCode::BAD_REQUEST, "unknown field: wat");
        assert_eq!(bad.class, ErrorClass::Protocol);
        let server = classify_status(reqwest::StatusCode::BAD_GATEWAY, "");
        assert_eq!(
            (server.class, server.retryable),
            (ErrorClass::Network, true)
        );
    }

    async fn read_request(sock: &mut tokio::net::TcpStream) {
        let mut buf = [0u8; 4096];
        let _ = tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf)).await;
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for body in [
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            ] {
                let (mut sock, _) = listener.accept().await.unwrap();
                read_request(&mut sock).await;
                sock.write_all(body.as_bytes()).await.unwrap();
            }
        });
        let client = reqwest::Client::new();
        let resp = post_sse(
            &client,
            &format!("http://{addr}/x"),
            &[],
            "{}".to_string(),
            5_000,
            3,
        )
        .await
        .unwrap();
        assert!(resp.status().is_success());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn non_retryable_fails_fast() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            read_request(&mut sock).await;
            sock.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let client = reqwest::Client::new();
        let err = post_sse(
            &client,
            &format!("http://{addr}/x"),
            &[],
            "{}".to_string(),
            5_000,
            3,
        )
        .await
        .unwrap_err();
        assert_eq!(err.class, ErrorClass::Auth);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_token_header_is_protocol_error() {
        // a control character in a token must hard-error naming the
        // header, never silently drop the auth header (undiagnosable
        // 401); multibyte UTF-8 is legal obs-text and still sends
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = reqwest::Client::new();
        let err = post_sse(
            &client,
            &format!("http://{addr}/x"),
            &[("x-api-key".to_string(), "bad\u{7}key".to_string())],
            "{}".to_string(),
            5_000,
            3,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.class,
            ErrorClass::Protocol,
            "unexpected class; message: {}",
            err.message
        );
        assert!(!err.retryable);
        assert!(err.message.contains("x-api-key"), "got: {}", err.message);
        // no request was sent: nothing arrives on the listener
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "no request must reach the server"
        );
    }
}
