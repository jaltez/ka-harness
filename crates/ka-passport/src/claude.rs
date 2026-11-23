//! Claude Pro/Max plan source: the one end-to-end passport provider
//! (ratified implementation order — "one provider end-to-end first, then
//! the long tail").
//!
//! Flow: PKCE S256 authorize at `https://claude.ai/oauth/authorize`
//! (public client `CLIENT_ID`, no secret), an ephemeral loopback
//! callback captures the `code`, the code exchanges at
//! `https://console.anthropic.com/v1/oauth/token`
//! (`grant_type = authorization_code`, then `refresh_token`). Tokens
//! persist as one JSON blob in the store (keyring service
//! `ka-passport`; [`crate::store::MemoryStore`] in tests), and
//! [`ClaudePlanSource::token`] refreshes proactively — any read within
//! [`REFRESH_MARGIN_SECS`] of expiry rotates the pair first. The access
//! token rides the anthropic_messages wire as `Authorization: Bearer`
//! plus the `oauth-2025-04-20` beta header (see
//! `ka_dialect::auth::ANTHROPIC_OAUTH_BETA`).

use std::sync::Arc;

use ka_dialect::auth::{AuthError, TokenSource};
use serde::{Deserialize, Serialize};

/// Vendor prefix served by this source (the `claude-plan/*` catalog
/// rows select it through `auth = "passport"`).
pub const VENDOR: &str = "claude-plan";

/// Public OAuth client id of the Claude CLI flow (a public client:
/// PKCE, no secret).
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Authorization endpoint (browser hop).
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";

/// Token endpoint (code exchange + refresh).
pub const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";

/// Offline access so a refresh token issues at all.
pub const SCOPES: &str = "org:create_api_key user:profile user:inference";

/// Refresh this many seconds before `expires_at` — a proactive margin,
/// so a token grabbed mid-turn never dies under the model's feet.
pub const REFRESH_MARGIN_SECS: u64 = 300;

/// How long `login` waits at the loopback callback for the browser.
const LOGIN_TIMEOUT_SECS: u64 = 300;

/// Loopback callback path the redirect_uri carries.
const CALLBACK_PATH: &str = "/callback";

/// Persisted credential set for the vendor (one JSON blob in the store).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// The bearer token for API calls.
    pub access_token: String,
    /// Long-lived rotation token (kept only in the store).
    pub refresh_token: String,
    /// Unix seconds the access token dies at.
    pub expires_at: u64,
}

/// Result of [`ClaudePlanSource::status`]: what the CLI `ka passport
/// status` surface reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Credentials exist in the store.
    pub logged_in: bool,
    /// Unix seconds the access token dies at (when logged in).
    pub expires_at: Option<u64>,
    /// Whether the access token is past its proactive-refresh margin.
    pub stale: bool,
}

/// The Claude Pro/Max [`TokenSource`].
pub struct ClaudePlanSource {
    store: Arc<dyn crate::store::TokenStore>,
    http: reqwest::Client,
    authorize_url: String,
    token_url: String,
    /// Serializes the load-refresh-save path so concurrent `token()`
    /// calls never stampede the refresh endpoint.
    refresh_lock: parking_lot::Mutex<()>,
}

impl ClaudePlanSource {
    /// Production source: OS keyring store, official endpoints.
    pub fn new() -> Self {
        Self::with_store(Arc::new(crate::store::KeyringStore))
    }

    /// Source over an injected store (tests / keyring-less fallback).
    pub fn with_store(store: Arc<dyn crate::store::TokenStore>) -> Self {
        Self {
            store,
            http: reqwest::Client::builder()
                // one refresh round-trip must never wedge a turn
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            authorize_url: AUTHORIZE_URL.to_string(),
            token_url: TOKEN_URL.to_string(),
            refresh_lock: parking_lot::Mutex::new(()),
        }
    }

    /// Point the OAuth endpoints somewhere else (tests: a local fake).
    pub fn with_endpoints(mut self, authorize_url: &str, token_url: &str) -> Self {
        self.authorize_url = authorize_url.to_string();
        self.token_url = token_url.to_string();
        self
    }

    /// The vendor prefix this source serves.
    pub fn vendor(&self) -> &'static str {
        VENDOR
    }

    /// The authorize URL for one login attempt (pure; the browser hop
    /// and callback server are `login`'s).
    pub fn authorize_url(&self, pkce: &crate::pkce::Pkce, redirect_uri: &str) -> String {
        format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&\
             code_challenge_method=S256&state={}",
            self.authorize_url,
            CLIENT_ID,
            urlencode(redirect_uri),
            urlencode(SCOPES),
            pkce.challenge,
            pkce.state,
        )
    }

    /// Full login flow (the `ka login claude` seam): print the authorize
    /// URL, best-effort open a browser, wait at an ephemeral loopback
    /// callback for the code, exchange it, persist. Returns the tokens.
    pub async fn login(&self) -> Result<Tokens, AuthError> {
        let pkce = crate::pkce::Pkce::generate();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| AuthError::Protocol {
                message: format!("loopback callback bind failed: {e}"),
            })?;
        let port = listener
            .local_addr()
            .map_err(|e| AuthError::Protocol {
                message: format!("loopback callback address unavailable: {e}"),
            })?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
        let url = self.authorize_url(&pkce, &redirect_uri);
        eprintln!("ka passport: open this URL to authorize Claude Pro/Max access:\n{url}");
        open_browser_best_effort(&url);
        let code = self.wait_for_code(&listener, &pkce.state).await?;
        let tokens = self.exchange(&code, &pkce.verifier, &redirect_uri).await?;
        self.save(&tokens)?;
        Ok(tokens)
    }

    /// Wait at the loopback listener for `?code=…&state=…` (one hit).
    /// The state must match — a mismatch or missing code is a protocol
    /// error, never a best-effort fallback.
    pub async fn wait_for_code(
        &self,
        listener: &tokio::net::TcpListener,
        expected_state: &str,
    ) -> Result<String, AuthError> {
        use tokio::io::AsyncReadExt as _;
        let wait = async {
            let (mut sock, _) = listener.accept().await.map_err(|e| AuthError::Protocol {
                message: format!("callback accept failed: {e}"),
            })?;
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            // read until the request head is complete (the browser sends
            // exactly one GET; the body is empty)
            loop {
                let n = sock.read(&mut tmp).await.map_err(|e| AuthError::Protocol {
                    message: format!("callback read failed: {e}"),
                })?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&buf);
            // answer the tab so the user can close it
            let page = "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\nconnection: close\r\n\r\n\
                        <h1>ka passport</h1><p>Login received — you can close this tab.</p>";
            use tokio::io::AsyncWriteExt as _;
            let _ = sock.write_all(page.as_bytes()).await;
            let _ = sock.shutdown().await;
            // `GET /callback?code=…&state=… HTTP/1.1`
            let target = text
                .split_whitespace()
                .nth(1)
                .ok_or_else(|| AuthError::Protocol {
                    message: "callback request had no path".to_string(),
                })?;
            let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
            let mut code = None;
            let mut state = None;
            for pair in query.split('&') {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                if k == "code" && !v.is_empty() {
                    code = Some(v.to_string());
                }
                if k == "state" {
                    state = Some(v.to_string());
                }
            }
            let code = code.ok_or_else(|| AuthError::Protocol {
                message: "callback carried no code (denied or malformed)".to_string(),
            })?;
            if state.as_deref() != Some(expected_state) {
                return Err(AuthError::Protocol {
                    message: "callback state mismatch (possible CSRF)".to_string(),
                });
            }
            Ok(code)
        };
        tokio::time::timeout(std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS), wait)
            .await
            .map_err(|_| AuthError::Protocol {
                message: format!("login callback timed out after {LOGIN_TIMEOUT_SECS}s"),
            })?
    }

    /// Exchange an authorization code for tokens (PKCE verifier proves
    /// the binding; no client secret exists). `redirect_uri` must be
    /// byte-identical to the one sent on the authorize hop (RFC 6749
    /// §4.1.3 — the provider compares exact strings).
    pub async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<Tokens, AuthError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ];
        self.token_request(&form, VENDOR, None).await
    }

    /// Rotate a token pair via the refresh grant.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, AuthError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ];
        self.token_request(&form, VENDOR, Some(refresh_token)).await
    }

    /// Shared POST to the token endpoint; the OAuth beta header rides
    /// the exchange/refresh per Anthropic's OAuth contract.
    /// `previous_refresh` is the refresh token already held: a response
    /// without a new one conserves the old (RFC 6749 §6 — the refresh
    /// token MAY be rotated, absence never means revocation). Storing
    /// `""` there would permanently log the user out on the next
    /// refresh.
    async fn token_request(
        &self,
        form: &[(&str, &str)],
        vendor: &str,
        previous_refresh: Option<&str>,
    ) -> Result<Tokens, AuthError> {
        let resp = self
            .http
            .post(&self.token_url)
            .header("anthropic-beta", ka_dialect::auth::ANTHROPIC_OAUTH_BETA)
            .form(form)
            .send()
            .await
            .map_err(|e| AuthError::Refresh {
                vendor: vendor.to_string(),
                message: format!("token endpoint unreachable: {e}"),
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            return Err(AuthError::Refresh {
                vendor: vendor.to_string(),
                message: format!("token endpoint returned {status}: {snippet}"),
            });
        }
        let parsed: TokenResponse = resp.json().await.map_err(|e| AuthError::Refresh {
            vendor: vendor.to_string(),
            message: format!("token response unreadable: {e}"),
        })?;
        let access = parsed.access_token.trim().to_string();
        if access.is_empty() {
            return Err(AuthError::Refresh {
                vendor: vendor.to_string(),
                message: "token response carried no access_token".to_string(),
            });
        }
        let now = unix_now();
        let expires_at = now.saturating_add(parsed.expires_in.unwrap_or(3600).max(1));
        let refresh_token = if parsed.refresh_token.trim().is_empty() {
            previous_refresh.unwrap_or("").to_string()
        } else {
            parsed.refresh_token.trim().to_string()
        };
        Ok(Tokens {
            access_token: access,
            refresh_token,
            expires_at,
        })
    }

    /// Persist the token blob (one key per vendor).
    fn save(&self, tokens: &Tokens) -> Result<(), AuthError> {
        let json = serde_json::to_string(tokens).map_err(|e| AuthError::Protocol {
            message: format!("token serialization failed: {e}"),
        })?;
        self.store
            .save(&store_key(VENDOR), &json)
            .map_err(|e| AuthError::Protocol { message: e })
    }

    /// Stored tokens, if any.
    fn stored(&self) -> Option<Tokens> {
        let json = self.store.load(&store_key(VENDOR))?;
        serde_json::from_str(&json).ok()
    }

    /// Clear stored credentials (already-logged-out stays success).
    pub fn logout(&self) -> Result<(), AuthError> {
        self.store
            .delete(&store_key(VENDOR))
            .map_err(|e| AuthError::Protocol { message: e })
    }

    /// Inspect the stored session without touching the network.
    pub fn status(&self) -> Status {
        match self.stored() {
            Some(tokens) => {
                let stale = unix_now() + REFRESH_MARGIN_SECS >= tokens.expires_at;
                Status {
                    logged_in: true,
                    expires_at: Some(tokens.expires_at),
                    stale,
                }
            }
            None => Status {
                logged_in: false,
                expires_at: None,
                stale: false,
            },
        }
    }

    /// The [`TokenSource::token`] core: serve a fresh-enough access
    /// token, refreshing (synchronously, via a transient local runtime)
    /// when the stored one is inside the margin or already dead.
    fn fresh_token(&self) -> Result<String, AuthError> {
        let _guard = self.refresh_lock.lock();
        let mut tokens = self.stored().ok_or_else(|| AuthError::NotLoggedIn {
            vendor: VENDOR.to_string(),
        })?;
        if unix_now() + REFRESH_MARGIN_SECS < tokens.expires_at {
            return Ok(tokens.access_token);
        }
        if tokens.refresh_token.is_empty() {
            return Err(AuthError::Refresh {
                vendor: VENDOR.to_string(),
                message: "token expired and no refresh token is stored".to_string(),
            });
        }
        let refreshed = self.refresh_blocking(&tokens.refresh_token.clone())?;
        tokens = refreshed;
        self.save(&tokens)?;
        Ok(tokens.access_token)
    }

    /// Run the async refresh on a scoped OS thread with a transient
    /// current-thread runtime. The [`TokenSource`] contract is sync and
    /// ka runs current-thread runtimes, so a runtime can never be
    /// started in place (tokio refuses `block_on` on a driving thread);
    /// the refresh path is rare (once per token lifetime), the ladder
    /// above it (`resolve_token` → wires) is sync by design.
    fn refresh_blocking(&self, refresh_token: &str) -> Result<Tokens, AuthError> {
        let slot = parking_lot::Mutex::new(None::<Result<Tokens, AuthError>>);
        let slot_ref = &slot;
        std::thread::scope(|s| {
            // the builder name only aids diagnostics; a spawn failure
            // still joins at scope end with an empty slot mapped below
            let _ = std::thread::Builder::new()
                .name("ka-passport-refresh".into())
                .spawn_scoped(s, move || {
                    let out = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt.block_on(self.refresh(refresh_token)),
                        Err(e) => Err(AuthError::Refresh {
                            vendor: VENDOR.to_string(),
                            message: format!("refresh runtime unavailable: {e}"),
                        }),
                    };
                    *slot_ref.lock() = Some(out);
                });
        });
        slot.into_inner().unwrap_or_else(|| {
            Err(AuthError::Refresh {
                vendor: VENDOR.to_string(),
                message: "refresh thread produced no result".to_string(),
            })
        })
    }
}

impl Default for ClaudePlanSource {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenSource for ClaudePlanSource {
    /// Bearer token for `vendor`, refreshing when stale. Only
    /// [`VENDOR`] is served.
    fn token(&self, vendor: &str) -> Result<String, AuthError> {
        if vendor != VENDOR {
            return Err(AuthError::NotLoggedIn {
                vendor: vendor.to_string(),
            });
        }
        self.fresh_token()
    }

    fn vendors(&self) -> &[String] {
        &VENDOR_LIST
    }
}

/// The served-vendor list, in the trait's `&[String]` shape.
static VENDOR_LIST: std::sync::LazyLock<Vec<String>> =
    std::sync::LazyLock::new(|| vec![VENDOR.to_string()]);

/// Construct the source and register it with the ka-dialect registry
/// (the harness `passport` feature calls this once at startup). Returns
/// the shared handle for the login/logout/status CLI.
pub fn install() -> Arc<ClaudePlanSource> {
    let source = Arc::new(ClaudePlanSource::new());
    ka_dialect::auth::register_token_source(source.clone());
    source
}

/// Token-endpoint response subset ka consumes.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    /// Seconds until the access token dies (provider-defined).
    expires_in: Option<u64>,
}

/// Store key for the vendor's credential blob.
fn store_key(vendor: &str) -> String {
    format!("{vendor}.tokens")
}

/// Unix seconds now (0 before the epoch — never fails).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Percent-encode everything outside the RFC 3986 unreserved set (a
/// redirect_uri carries `:` and `/`; scopes carry spaces).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Best-effort `xdg-open`/`open`/`start` on the authorize URL. The URL
/// is always printed first, so a headless or odd-desktop environment
/// still completes the flow by hand; a failure here is silent.
fn open_browser_best_effort(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/c", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    // fire and forget; the URL was printed regardless
    let _ = std::process::Command::new(program).args(args).spawn();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::store::{MemoryStore, TokenStore};

    /// A source over memory storage with token-exchange endpoints
    /// pointed at a local fake (caller supplies the URLs).
    fn test_source(store: Arc<MemoryStore>, token_url: &str) -> ClaudePlanSource {
        ClaudePlanSource::with_store(store)
            .with_endpoints("http://127.0.0.1:1/authorize", token_url)
    }

    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let src = ClaudePlanSource::with_store(Arc::new(MemoryStore::default()));
        let pkce = crate::pkce::Pkce::generate();
        let url = src.authorize_url(&pkce, "http://127.0.0.1:5173/callback");
        assert!(url.starts_with(AUTHORIZE_URL), "authorize endpoint: {url}");
        for piece in [
            "response_type=code",
            &format!("client_id={CLIENT_ID}"),
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A5173%2Fcallback",
            &format!("code_challenge={}", pkce.challenge),
            "code_challenge_method=S256",
            &format!("state={}", pkce.state),
            &format!("scope={}", urlencode(SCOPES)),
        ] {
            assert!(url.contains(piece), "missing {piece:?} in {url}");
        }
    }

    #[test]
    fn urlencode_escapes_reserved_bytes() {
        assert_eq!(urlencode("a b&c=d/e"), "a%20b%26c%3Dd%2Fe");
        assert_eq!(urlencode("plain-string_1.2~3"), "plain-string_1.2~3");
    }

    #[test]
    fn status_reflects_store_state() {
        let store = Arc::new(MemoryStore::default());
        let src = test_source(store.clone(), "http://127.0.0.1:1/token");
        assert_eq!(
            src.status(),
            Status {
                logged_in: false,
                expires_at: None,
                stale: false
            }
        );
        store
            .save(
                &store_key(VENDOR),
                &serde_json::to_string(&Tokens {
                    access_token: "at".into(),
                    refresh_token: "rt".into(),
                    expires_at: unix_now() + 3_600,
                })
                .unwrap(),
            )
            .unwrap();
        let st = src.status();
        assert!(st.logged_in);
        assert!(!st.stale, "an hour out is fresh");
        // stale: inside the proactive margin
        store
            .save(
                &store_key(VENDOR),
                &serde_json::to_string(&Tokens {
                    access_token: "at".into(),
                    refresh_token: "rt".into(),
                    expires_at: unix_now() + 10,
                })
                .unwrap(),
            )
            .unwrap();
        assert!(src.status().stale);
        // logout clears
        src.logout().unwrap();
        assert!(!src.status().logged_in);
        // logout on an empty store stays success
        src.logout().unwrap();
    }

    #[test]
    fn token_without_credentials_is_not_logged_in() {
        let src = ClaudePlanSource::with_store(Arc::new(MemoryStore::default()));
        let err = TokenSource::token(&src, VENDOR).unwrap_err();
        assert_eq!(
            err,
            AuthError::NotLoggedIn {
                vendor: VENDOR.into()
            }
        );
        // other vendors are never served
        let err = TokenSource::token(&src, "someone-else").unwrap_err();
        assert_eq!(
            err,
            AuthError::NotLoggedIn {
                vendor: "someone-else".into()
            }
        );
    }

    #[test]
    fn vendors_lists_only_claude_plan() {
        let src = ClaudePlanSource::with_store(Arc::new(MemoryStore::default()));
        assert_eq!(src.vendors(), &[VENDOR.to_string()]);
    }

    /// Local OAuth-token fake: asserts the request is a proper token
    /// exchange/refresh, answers one JSON blob, records the body. The
    /// listener lives in the spawned task for the rest of the test.
    async fn serve_token_json(
        body: &'static str,
        captured: Arc<parking_lot::Mutex<Vec<String>>>,
    ) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if request_complete(&buf) {
                    break;
                }
            }
            captured
                .lock()
                .push(String::from_utf8_lossy(&buf).into_owned());
            let http = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(http.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
        });
        format!("http://{addr}/token")
    }

    /// Same completion rule as the ka-dialect wire fixtures.
    fn request_complete(buf: &[u8]) -> bool {
        let Ok(s) = std::str::from_utf8(buf) else {
            return false;
        };
        let Some(pos) = s.find("\r\n\r\n") else {
            return false;
        };
        s.to_ascii_lowercase()
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .is_none_or(|cl| buf.len() >= pos + 4 + cl)
    }

    #[tokio::test]
    async fn wait_for_code_verifies_state_and_extracts_code() {
        use tokio::io::AsyncWriteExt as _;
        let src = ClaudePlanSource::with_store(Arc::new(MemoryStore::default()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

        // a browser-shaped GET lands on the callback
        let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        client
            .write_all(b"GET /callback?code=abc123&state=st-42 HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        let code = src.wait_for_code(&listener, "st-42").await.unwrap();
        assert_eq!(code, "abc123");

        // state mismatch is a protocol error, not a code
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        client
            .write_all(b"GET /callback?code=abc123&state=wrong HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        let err = src.wait_for_code(&listener, "st-42").await.unwrap_err();
        assert!(matches!(err, AuthError::Protocol { .. }), "got: {err:?}");

        // no code at all (user denied) is a protocol error
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        client
            .write_all(b"GET /callback?error=access_denied&state=st-42 HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        let err = src.wait_for_code(&listener, "st-42").await.unwrap_err();
        assert!(matches!(err, AuthError::Protocol { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn exchange_sends_pkce_form_and_stores_tokens() {
        let store = Arc::new(MemoryStore::default());
        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let url = serve_token_json(
            r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":7200}"#,
            captured.clone(),
        )
        .await;
        let src = test_source(store.clone(), &url);
        let tokens = src
            .exchange("the-code", "the-verifier", "http://127.0.0.1:5173/callback")
            .await
            .unwrap();
        assert_eq!(tokens.access_token, "at-1");
        assert_eq!(tokens.refresh_token, "rt-1");
        let raw = captured.lock()[0].clone();
        assert!(raw.contains("grant_type=authorization_code"), "body: {raw}");
        assert!(raw.contains("code=the-code"), "body: {raw}");
        assert!(
            raw.contains(&format!("client_id={CLIENT_ID}")),
            "body: {raw}"
        );
        assert!(raw.contains("code_verifier=the-verifier"), "body: {raw}");
        // RFC 6749 §4.1.3: the exchange redirect_uri must be byte-
        // identical to the authorize hop's — the full loopback URL rides
        // the form, not an empty placeholder
        assert!(
            raw.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5173%2Fcallback"),
            "exchange carries the authorize redirect_uri: {raw}"
        );
        assert!(
            !raw.contains("redirect_uri=&"),
            "no empty redirect_uri: {raw}"
        );
        assert!(
            raw.contains(&format!(
                "anthropic-beta: {}",
                ka_dialect::auth::ANTHROPIC_OAUTH_BETA
            )),
            "OAuth beta header rides the exchange: {raw}"
        );
        // persistence is login's job; save then read back through status
        src.save(&tokens).unwrap();
        let st = src.status();
        assert!(st.logged_in);
        assert!(st.expires_at.unwrap() > unix_now() + 3_600);
    }

    #[tokio::test]
    async fn refresh_without_rotation_conserves_previous_refresh_token() {
        // RFC 6749 §6: a refresh response may omit refresh_token — that
        // is "keep the old one", never revocation. Storing "" would
        // permanently log the user out on the next refresh.
        let store = Arc::new(MemoryStore::default());
        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let url = serve_token_json(
            r#"{"access_token":"at-3","expires_in":7200}"#,
            captured.clone(),
        )
        .await;
        let src = test_source(store.clone(), &url);
        let tokens = src.refresh("rt-old").await.unwrap();
        assert_eq!(tokens.access_token, "at-3");
        assert_eq!(
            tokens.refresh_token, "rt-old",
            "rotation-less response conserves the old refresh token"
        );
        // an explicitly empty refresh_token is conserved too
        let url = serve_token_json(
            r#"{"access_token":"at-4","refresh_token":"  ","expires_in":7200}"#,
            captured.clone(),
        )
        .await;
        let src = test_source(store.clone(), &url);
        let tokens = src.refresh("rt-old").await.unwrap();
        assert_eq!(tokens.refresh_token, "rt-old");
    }

    #[tokio::test]
    async fn stale_token_refreshes_proactively() {
        let store = Arc::new(MemoryStore::default());
        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let url = serve_token_json(
            r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":7200}"#,
            captured.clone(),
        )
        .await;
        // seed an inside-margin session
        store
            .save(
                &store_key(VENDOR),
                &serde_json::to_string(&Tokens {
                    access_token: "at-old".into(),
                    refresh_token: "rt-old".into(),
                    expires_at: unix_now() + 10,
                })
                .unwrap(),
            )
            .unwrap();
        let src = test_source(store.clone(), &url);
        // the sync TokenSource::token blocks its calling thread while
        // the refresh runs on its own runtime — drive it on the blocking
        // pool so this current_thread test runtime keeps serving the
        // fixture (production blocks the engine thread the same way)
        let src = Arc::new(src);
        let src2 = Arc::clone(&src);
        let token = tokio::task::spawn_blocking(move || TokenSource::token(src2.as_ref(), VENDOR))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(token, "at-2", "stale session rotated before serving");
        let raw = captured.lock()[0].clone();
        assert!(raw.contains("grant_type=refresh_token"), "body: {raw}");
        assert!(raw.contains("refresh_token=rt-old"), "body: {raw}");
        // the rotated pair replaced the stored one
        let st = src.status();
        assert!(!st.stale);
    }

    #[tokio::test]
    async fn fresh_token_serves_without_network() {
        let store = Arc::new(MemoryStore::default());
        let src = test_source(store.clone(), "http://127.0.0.1:1/token");
        store
            .save(
                &store_key(VENDOR),
                &serde_json::to_string(&Tokens {
                    access_token: "at-fresh".into(),
                    refresh_token: "rt".into(),
                    expires_at: unix_now() + REFRESH_MARGIN_SECS * 10,
                })
                .unwrap(),
            )
            .unwrap();
        // no listener exists at the fake URL: a network attempt would
        // fail the test via the returned error, not a panic
        assert_eq!(TokenSource::token(&src, VENDOR).unwrap(), "at-fresh");
    }

    #[tokio::test]
    async fn failed_refresh_surfaces_auth_error() {
        let store = Arc::new(MemoryStore::default());
        // port 1 refuses connections: the refresh maps to AuthError
        let src = test_source(store.clone(), "http://127.0.0.1:1/token");
        store
            .save(
                &store_key(VENDOR),
                &serde_json::to_string(&Tokens {
                    access_token: "at-old".into(),
                    refresh_token: "rt-old".into(),
                    expires_at: unix_now() + 10,
                })
                .unwrap(),
            )
            .unwrap();
        let err = tokio::task::spawn_blocking({
            let src = Arc::new(src);
            move || TokenSource::token(src.as_ref(), VENDOR)
        })
        .await
        .unwrap()
        .unwrap_err();
        assert!(matches!(err, AuthError::Refresh { .. }), "got: {err:?}");
    }
}
