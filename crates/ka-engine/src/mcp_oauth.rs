//! EXT-03: OAuth 2.0 for MCP HTTP servers — authorization-server
//! metadata discovery (RFC 8414/9728-style), dynamic client
//! registration (RFC 7591) or a pre-registered CIMD client id, the
//! authorization-code flow with PKCE (S256) through a loopback
//! callback, and token caching with refresh grants.
//!
//! Security posture:
//! - tokens live in `<data>/mcp-tokens/<server-hash>.json`, written
//!   with mode 0600 (owner read/write only); the directory is 0700;
//! - PKCE with an S256 challenge on every authorization request (no
//!   client secret is stored for public clients);
//! - a random per-flow `state` is matched against the callback;
//! - `bearer_token_env` disables the whole flow — a user-provided
//!   token always wins, and nothing interactive happens.
//!
//! No new dependencies: SHA-256 and base64url are implemented here
//! (and pinned by known-answer tests), randomness comes from
//! `/dev/urandom`.

use std::hash::{DefaultHasher, Hasher};
use std::path::PathBuf;

use serde_json::json;
use tokio::sync::mpsc;

/// The authorize-URL wait: how long the loopback callback listens for
/// the browser redirect before the flow gives up.
pub const DEFAULT_AUTHORIZE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// EXP-03b cap: a headers_helper emitting more than 8 KiB is an error,
/// never a truncation (a silently cut header map is a footgun).
pub const HEADERS_HELPER_CAP: usize = 8 * 1024;

/// Access + refresh tokens for one server, as cached on disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenSet {
    /// The bearer token for `Authorization: Bearer …`.
    pub access_token: String,
    /// The refresh grant, when the server issued one.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// The OAuth client id this token set belongs to (DCR ids are
    /// server-issued; CIMD ids come from the config). Cached so a
    /// reconnect can refresh without re-registering.
    #[serde(default)]
    pub client_id: Option<String>,
    /// Epoch seconds at which the access token expires (already
    /// skewed 60 s early); `None` = the server sent no expiry (treat
    /// as unexpired until a 401 says otherwise).
    #[serde(default)]
    pub expires_at: Option<u64>,
}

impl TokenSet {
    /// Whether the access token is expired (or expiring within the
    /// 60 s skew window).
    pub fn expired(&self) -> bool {
        self.expires_at.is_some_and(|at| at <= now_epoch_secs())
    }

    /// Build from a token-endpoint response (`expires_in` skewed 60 s
    /// early so in-flight requests never straddle the deadline).
    fn from_response(v: &serde_json::Value) -> Result<Self, String> {
        let access_token = v["access_token"]
            .as_str()
            .ok_or_else(|| "token response has no access_token".to_string())?
            .to_string();
        let expires_at = v["expires_in"]
            .as_u64()
            .map(|secs| now_epoch_secs().saturating_add(secs.saturating_sub(60)));
        Ok(Self {
            access_token,
            refresh_token: v["refresh_token"].as_str().map(str::to_string),
            client_id: None,
            expires_at,
        })
    }
}

/// RFC 8414-style authorization-server metadata (the subset ka uses).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AuthServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// RFC 7591 dynamic registration endpoint, when offered.
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    /// Default scope set when the config names none.
    #[serde(default)]
    pub scopes_supported: Option<Vec<String>>,
}

/// Everything the flow needs to talk to the user and the test bench.
#[derive(Clone, Default)]
pub struct FlowHooks {
    /// Progress lines surfaced as transcript Notes (the authorize URL
    /// lands here).
    pub notes: Option<mpsc::UnboundedSender<String>>,
    /// Tests drive the callback themselves: headless flows are
    /// allowed when true. The engine sets it from `is_terminal()`.
    pub allow_headless: bool,
    /// Code-wait timeout (tests shrink it).
    pub authorize_timeout: std::time::Duration,
}

impl FlowHooks {
    /// Surface one progress line best-effort.
    fn note(&self, message: String) {
        if let Some(tx) = &self.notes {
            tx.send(message).ok();
        }
    }
}

/// Where the token cache lives: `$XDG_DATA_HOME/ka/mcp-tokens/`
/// (fallback `~/.local/share/ka/mcp-tokens/`, temp-dir last resort).
fn tokens_dir() -> PathBuf {
    #[cfg(any(test, feature = "test-util"))]
    if let Some(dir) = test_support::current_dir() {
        return dir;
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = PathBuf::from(h);
                p.push(".local/share");
                p
            })
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("ka").join("mcp-tokens")
}

/// Stable cache key for a server: a hash over the authorization
/// identity (name + url), not the whole launch config — rotating
/// headers must not orphan tokens.
pub fn server_key(cfg: &crate::mcp::McpServerConfig) -> String {
    let mut h = DefaultHasher::new();
    h.write(cfg.name.as_bytes());
    h.write(cfg.url.as_deref().unwrap_or("").as_bytes());
    format!("{:016x}", h.finish())
}

/// The cached token set for `key`, when present and parseable.
/// Corrupt files are misses (a bad token file must not wedge the
/// connect path).
pub fn load_tokens(key: &str) -> Option<TokenSet> {
    let text = std::fs::read_to_string(tokens_dir().join(format!("{key}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

/// Persist `tokens` for `key` best-effort: 0600 file inside a 0700
/// directory. A failed write costs a re-login next time, nothing more.
pub fn store_tokens(key: &str, tokens: &TokenSet) {
    let dir = tokens_dir();
    if let Err(_e) = std::fs::create_dir_all(&dir) {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let Ok(text) = serde_json::to_string(tokens) else {
        return;
    };
    let path = dir.join(format!("{key}.json"));
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
    else {
        return;
    };
    use std::io::Write;
    if f.write_all(text.as_bytes()).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
    }
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- crypto

/// SHA-256 (FIPS 180-4), hand-rolled to stay inside the dependency
/// budget; pinned by known-answer tests below.
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// URL-safe base64 without padding (RFC 4648 §5).
pub(crate) fn base64url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// Random bytes from `/dev/urandom`, falling back to a
/// time+pid-seeded xorshift (tests and locked-down containers).
fn random_bytes(n: usize) -> Vec<u8> {
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let mut buf = vec![0u8; n];
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15)
        ^ ((std::process::id() as u64) << 32)
        | 1;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        out.push((seed.wrapping_mul(0x2545F4914F6CDD1D) >> 24) as u8);
    }
    out
}

// ---------------------------------------------------------- flow steps

/// Discover authorization-server metadata for the resource at `base`:
/// `{origin}/.well-known/oauth-authorization-server` first, the
/// RFC 8414 path-aware form second.
pub async fn discover_metadata(
    client: &reqwest::Client,
    base: &str,
) -> Result<AuthServerMetadata, String> {
    let (origin, path) = split_origin_path(base);
    let mut candidates = vec![format!("{origin}/.well-known/oauth-authorization-server")];
    if !path.is_empty() {
        candidates.push(format!(
            "{origin}/.well-known/{path}/oauth-authorization-server"
        ));
    }
    let mut last = String::new();
    for url in candidates {
        let resp = client
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("oauth discovery: {e}"))?;
        if !resp.status().is_success() {
            last = format!("oauth discovery: status {} from {url}", resp.status());
            continue;
        }
        let body = resp
            .text()
            .await
            .map_err(|e| format!("oauth discovery: {e}"))?;
        return serde_json::from_str(&body)
            .map_err(|e| format!("oauth discovery: malformed metadata from {url}: {e}"));
    }
    Err(last)
}

/// Split `scheme://host[:port]/path` into origin and non-root path.
fn split_origin_path(url: &str) -> (String, String) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_string(), String::new());
    };
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, p),
        None => (rest, ""),
    };
    (
        format!("{scheme}://{host}"),
        path.trim_end_matches('/').to_string(),
    )
}

/// RFC 7591 dynamic client registration: a public client (no secret)
/// that authorizes via code + PKCE and refreshes.
async fn register_client(
    client: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
    scopes: &[String],
) -> Result<String, String> {
    let mut body = json!({
        "client_name": "ka",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if !scopes.is_empty() {
        body["scope"] = json!(scopes.join(" "));
    }
    let resp = client
        .post(registration_endpoint)
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("oauth registration: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("oauth registration: {e}"))?;
    if !status.is_success() {
        return Err(format!("oauth registration: status {status} — {text}"));
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("oauth registration: malformed response: {e}"))?;
    v["client_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "oauth registration: response has no client_id".to_string())
}

/// Percent-encode everything outside the RFC 3986 unreserved set.
fn pct_encode(s: &str) -> String {
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

/// Build the authorize URL (response_type=code, PKCE S256, state).
pub fn authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    state: &str,
    code_challenge: &str,
) -> String {
    let mut params: Vec<(String, String)> = vec![
        ("response_type".into(), "code".into()),
        ("client_id".into(), client_id.into()),
        ("redirect_uri".into(), redirect_uri.into()),
        ("state".into(), state.into()),
        ("code_challenge".into(), code_challenge.into()),
        ("code_challenge_method".into(), "S256".into()),
    ];
    if !scopes.is_empty() {
        params.push(("scope".into(), scopes.join(" ")));
    }
    let query: Vec<String> = params
        .into_iter()
        .map(|(k, v)| format!("{}={}", pct_encode(&k), pct_encode(&v)))
        .collect();
    let sep = if authorization_endpoint.contains('?') {
        "&"
    } else {
        "?"
    };
    format!("{authorization_endpoint}{sep}{}", query.join("&"))
}

/// Read one HTTP request off the callback connection (headers + body);
/// returns the request target.
async fn read_callback_request(sock: &mut tokio::net::TcpStream) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = sock
            .read(&mut tmp)
            .await
            .map_err(|e| format!("oauth callback read: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        let text = String::from_utf8_lossy(&buf);
        if let Some(head_end) = text.find("\r\n\r\n") {
            let cl = text
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buf.len() >= head_end + 4 + cl {
                break;
            }
        }
        if buf.len() > 64 * 1024 {
            return Err("oauth callback: oversized request".to_string());
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let target = text
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "oauth callback: malformed request line".to_string())?
        .to_string();
    Ok(target)
}

/// Parse `?code=…&state=…` out of a callback target.
fn parse_callback(target: &str) -> Result<(String, String), String> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or(target);
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = v
            .replace("%3D", "=")
            .replace("%2F", "/")
            .replace("%2B", "+");
        match k {
            "code" => code = Some(v.clone()),
            "state" => state = Some(v),
            _ => {}
        }
    }
    match (code, state) {
        (Some(code), Some(state)) => Ok((code, state)),
        _ => Err("oauth callback: redirect carried no code+state".to_string()),
    }
}

/// Token-endpoint POST shared by the code exchange and refresh grants.
async fn token_request(
    client: &reqwest::Client,
    token_endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenSet, String> {
    let form_pairs: Vec<(String, String)> = form
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let resp = client
        .post(token_endpoint)
        .header("Accept", "application/json")
        .form(&form_pairs)
        .send()
        .await
        .map_err(|e| format!("oauth token: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("oauth token: {e}"))?;
    if !status.is_success() {
        return Err(format!("oauth token: status {status} — {text}"));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("oauth token: malformed response: {e}"))?;
    TokenSet::from_response(&v)
}

/// Exchange the authorization code for tokens (PKCE verifier bound).
pub async fn exchange_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenSet, String> {
    token_request(
        client,
        token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", code_verifier),
        ],
    )
    .await
}

/// Refresh grant: rotate to a fresh token set.
pub async fn refresh_tokens(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
    scopes: &[String],
) -> Result<TokenSet, String> {
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    let scope_joined;
    if !scopes.is_empty() {
        scope_joined = scopes.join(" ");
        form.push(("scope", scope_joined.as_str()));
    }
    token_request(client, token_endpoint, &form).await
}

/// The full interactive authorization: discover → register (or CIMD)
/// → loopback callback → exchange → cache. Returns the fresh tokens
/// plus the client id that owns them (DCR-issued or CIMD).
///
/// Without a TTY (and without [`FlowHooks::allow_headless`], the test
/// escape) the flow fails up front with the `ka mcp login` instruction
/// — never a silent browser-less hang.
pub async fn authorize(
    client: &reqwest::Client,
    server_name: &str,
    resource_base: &str,
    scopes: &[String],
    cimd_client_id: Option<&str>,
    hooks: &FlowHooks,
) -> Result<(TokenSet, String), String> {
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if !interactive && !hooks.allow_headless {
        return Err(format!(
            "mcp {server_name}: OAuth needs an interactive terminal for the browser \
             authorization — run `ka mcp login {server_name}`, or set bearer_token_env \
             to skip OAuth"
        ));
    }
    let metadata = discover_metadata(client, resource_base).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("oauth callback bind: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("oauth callback bind: {e}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let client_id = match cimd_client_id {
        Some(id) => id.to_string(),
        None => {
            let reg = metadata.registration_endpoint.as_deref().ok_or_else(|| {
                format!(
                    "mcp {server_name}: no dynamic-registration endpoint advertised and \
                     no oauth_client_id configured — pre-register a client (CIMD) or set \
                     oauth_client_id"
                )
            })?;
            register_client(client, reg, &redirect_uri, scopes).await?
        }
    };

    let verifier_bytes = random_bytes(32);
    let code_verifier = base64url(&verifier_bytes);
    let challenge = base64url(&sha256(code_verifier.as_bytes()));
    let state = base64url(&random_bytes(16));
    let url = authorize_url(
        &metadata.authorization_endpoint,
        &client_id,
        &redirect_uri,
        scopes,
        &state,
        &challenge,
    );
    hooks.note(format!(
        "mcp {server_name}: authorize in your browser, then ka waits on the callback:\n  {url}"
    ));

    let code = tokio::time::timeout(hooks.authorize_timeout, async {
        let (mut sock, _) = listener
            .accept()
            .await
            .map_err(|e| format!("oauth callback accept: {e}"))?;
        let target = read_callback_request(&mut sock).await?;
        let _ = tokio::io::AsyncWriteExt::write_all(
            &mut sock,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 52\r\nConnection: close\r\n\r\nka: authorization received - you can close this tab.\n",
        )
        .await;
        let (code, got_state) = parse_callback(&target)?;
        if got_state != state {
            return Err("oauth callback: state mismatch (possible CSRF) — aborting".to_string());
        }
        Ok(code)
    })
    .await
    .map_err(|_| {
        format!(
            "mcp {server_name}: no OAuth callback within {}s — restart the connect or \
             run `ka mcp login {server_name}`",
            hooks.authorize_timeout.as_secs()
        )
    })??;

    let tokens = exchange_code(
        client,
        &metadata.token_endpoint,
        &client_id,
        &code,
        &redirect_uri,
        &code_verifier,
    )
    .await?;
    Ok((tokens, client_id))
}

/// EXT-03b: run a headers_helper command and parse its JSON-object
/// stdout into headers. Blocking — callers wrap in spawn_blocking.
/// Output past [`HEADERS_HELPER_CAP`] is an error (inline, with the
/// captured stderr when the command itself failed).
pub fn run_headers_helper(command: &str) -> Result<Vec<(String, String)>, String> {
    use std::process::{Command, Stdio};
    let out = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("headers_helper: spawn failed: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!(
            "headers_helper failed (exit {}): {}",
            out.status.code().unwrap_or(-1),
            stderr.trim()
        ));
    }
    if out.stdout.len() > HEADERS_HELPER_CAP {
        return Err(format!(
            "headers_helper emitted {} bytes — over the {} cap",
            out.stdout.len(),
            HEADERS_HELPER_CAP
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|e| format!("headers_helper: stdout is not a JSON object: {e}"))?;
    let map = v.as_object().ok_or_else(|| {
        "headers_helper: stdout must be a JSON object of header names to values".to_string()
    })?;
    let mut headers = Vec::new();
    for (k, val) in map {
        let value = match val {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        headers.push((k.clone(), value));
    }
    Ok(headers)
}

/// Ensure fresh tokens for `cfg`: cache hit that is still valid → as
/// is; expired with a refresh token → one refresh attempt (result
/// re-cached, a failed refresh falls through to a fresh
/// authorization); anything else → the interactive flow. Returns the
/// tokens and the authorization-server metadata (the transport keeps
/// the token endpoint for its 401 refreshes).
pub async fn ensure_tokens(
    client: &reqwest::Client,
    cfg: &crate::mcp::McpServerConfig,
    hooks: &FlowHooks,
) -> Result<(TokenSet, AuthServerMetadata), String> {
    let url = cfg
        .url
        .as_deref()
        .ok_or_else(|| "oauth applies to url servers only".to_string())?;
    let key = server_key(cfg);
    let metadata = discover_metadata(client, url).await?;
    if let Some(mut tokens) = load_tokens(&key) {
        if tokens.client_id.is_none() {
            tokens.client_id = cfg.oauth_client_id.clone();
            store_tokens(&key, &tokens);
        }
        if !tokens.expired() {
            return Ok((tokens, metadata));
        }
        if let (Some(refresh), Some(client_id)) =
            (tokens.refresh_token.clone(), tokens.client_id.clone())
        {
            // one refresh attempt; a rejected grant (revoked/expired
            // refresh token) falls through to a fresh authorization
            if let Ok(mut fresh) = refresh_tokens(
                client,
                &metadata.token_endpoint,
                &client_id,
                &refresh,
                &cfg.oauth_scopes,
            )
            .await
            {
                if fresh.client_id.is_none() {
                    fresh.client_id = Some(client_id);
                }
                store_tokens(&key, &fresh);
                return Ok((fresh, metadata));
            }
        }
    }
    let scopes = if cfg.oauth_scopes.is_empty() {
        metadata.scopes_supported.clone().unwrap_or_default()
    } else {
        cfg.oauth_scopes.clone()
    };
    let (mut tokens, client_id) = authorize(
        client,
        &cfg.name,
        url,
        &scopes,
        cfg.oauth_client_id.as_deref(),
        hooks,
    )
    .await?;
    if tokens.client_id.is_none() {
        tokens.client_id = Some(client_id);
    }
    store_tokens(&key, &tokens);
    Ok((tokens, metadata))
}

#[cfg(any(test, feature = "test-util"))]
pub mod test_support {
    //! Test-only redirection of the token-cache directory (the
    //! trust-store idiom: no env mutation, process-global, serialized).

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;

    static DIR: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);
    static LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    pub fn current_dir() -> Option<PathBuf> {
        DIR.lock().clone()
    }

    /// Redirect the token cache to a fresh temp dir until the guard
    /// drops; usable across `.await` points.
    pub fn tokens_guard() -> TokensGuard {
        let lock = LOCK.lock();
        let dir = std::env::temp_dir().join(format!(
            "ka-mcptokens-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        *DIR.lock() = Some(dir.clone());
        TokensGuard {
            dir: Some(dir),
            _lock: Some(lock),
        }
    }

    pub struct TokensGuard {
        dir: Option<PathBuf>,
        _lock: Option<parking_lot::MutexGuard<'static, ()>>,
    }

    impl Drop for TokensGuard {
        fn drop(&mut self) {
            *DIR.lock() = None;
            if let Some(dir) = self.dir.take() {
                let _ = std::fs::remove_dir_all(dir);
            }
            drop(self._lock.take());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::mcp::McpClient;
    use serde_json::{Value, json};
    use tokio::io::AsyncWriteExt;

    #[test]
    fn sha256_matches_known_vectors() {
        let hex = |d: [u8; 32]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(
            hex(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // > one block, exercises padding across 2+ chunks
        let long = vec![b'a'; 1000];
        assert_eq!(
            hex(sha256(&long)),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn base64url_is_unpadded_and_url_safe() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        // RFC 4648 test vectors end; no '+' or '/' may ever appear
        assert!(!base64url(&[0xfb, 0xff, 0xbf, 0xff]).contains('+'));
    }

    /// RFC 7636 appendix B: the S256 challenge for this verifier.
    #[test]
    fn pkce_challenge_matches_rfc7636_appendix_b() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64url(&sha256(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let url = authorize_url(
            "https://auth.example/authorize",
            "client-1",
            "http://127.0.0.1:9/callback",
            &["mcp:read".into(), "mcp:write".into()],
            "st-1",
            "challenge-1",
        );
        assert!(url.starts_with("https://auth.example/authorize?"));
        for part in [
            "response_type=code",
            "client_id=client-1",
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcallback",
            "state=st-1",
            "code_challenge=challenge-1",
            "code_challenge_method=S256",
            "scope=mcp%3Aread%20mcp%3Awrite",
        ] {
            assert!(url.contains(part), "{part} missing from {url}");
        }
        // a URL already carrying a query appends with &
        let url2 = authorize_url("https://a/e?x=1", "c", "http://r", &[], "s", "cc");
        assert!(url2.contains("?x=1&response_type=code"));
    }

    #[test]
    fn callback_parse_extracts_code_and_state() {
        let (code, state) = parse_callback("/callback?code=abc123&state=xyz").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state, "xyz");
        assert!(parse_callback("/callback?error=access_denied").is_err());
    }

    #[test]
    fn token_cache_roundtrip_is_0600_and_key_stable() {
        let _g = test_support::tokens_guard();
        let mut cfg = crate::mcp::McpServerConfig {
            name: "demo".into(),
            command: None,
            args: Vec::new(),
            env: Default::default(),
            url: Some("https://mcp.example/mcp".into()),
            headers: Vec::new(),
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            approval: None,
            tool_timeout_secs: None,
            output_limit: None,
            oauth: Some(true),
            oauth_client_id: None,
            oauth_scopes: vec!["mcp".into()],
            bearer_token_env: None,
            headers_helper: None,
        };
        let key = server_key(&cfg);
        let tokens = TokenSet {
            access_token: "at-1".into(),
            refresh_token: Some("rt-1".into()),
            client_id: Some("client-42".into()),
            expires_at: Some(now_epoch_secs() + 3600),
        };
        store_tokens(&key, &tokens);
        let back = load_tokens(&key).unwrap();
        assert_eq!(back.access_token, "at-1");
        assert_eq!(back.refresh_token.as_deref(), Some("rt-1"));
        assert!(!back.expired());
        // 0600 on the file, 0700 on the dir
        use std::os::unix::fs::PermissionsExt;
        let path = tokens_dir().join(format!("{key}.json"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(tokens_dir())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        // a header change does not orphan the tokens, a url change does
        cfg.headers.push(("x".into(), "y".into()));
        assert_eq!(server_key(&cfg), key);
        cfg.url = Some("https://other.example/mcp".into());
        assert_ne!(server_key(&cfg), key);
    }

    #[test]
    fn expiry_skew_and_missing_expiry() {
        let expired = TokenSet {
            access_token: "a".into(),
            refresh_token: None,
            client_id: None,
            expires_at: Some(now_epoch_secs().saturating_sub(1)),
        };
        assert!(expired.expired());
        let open = TokenSet {
            access_token: "a".into(),
            refresh_token: None,
            client_id: None,
            expires_at: None,
        };
        assert!(!open.expired(), "no expires_in reads as unexpired");
    }

    #[test]
    fn headers_helper_parses_json_and_caps_and_reports_errors() {
        let hs = run_headers_helper("printf '{\"x-a\": \"1\", \"x-b\": 2}'").unwrap();
        assert_eq!(
            hs,
            vec![
                ("x-a".to_string(), "1".to_string()),
                ("x-b".to_string(), "2".to_string())
            ]
        );
        // failing command: stderr comes back inline
        let err = run_headers_helper("echo boom >&2; exit 3").unwrap_err();
        assert!(err.contains("boom") && err.contains("3"), "{err}");
        // over-cap output is an error, never a truncation
        let err = run_headers_helper("head -c 9000 /dev/zero | base64 | tr -d '\\n'").unwrap_err();
        assert!(err.contains("over the 8192 cap"), "{err}");
        // non-JSON output is an inline error too
        let err = run_headers_helper("echo not-json").unwrap_err();
        assert!(err.contains("not a JSON object"), "{err}");
    }

    // ------------------------------------------------- fixture flow

    /// Shared fixture state: what the fake MCP+OAuth server saw.
    #[derive(Default)]
    struct FixtureState {
        /// Request targets in arrival order (discovery hits included).
        paths: Vec<String>,
        /// The `Authorization` header of every `/mcp` request.
        bearer_seen: Vec<Option<String>>,
        /// Token-endpoint bodies (form-decoded).
        grants: Vec<std::collections::BTreeMap<String, String>>,
        /// The DCR request body, when registration ran.
        registered: Option<Value>,
    }

    impl FixtureState {
        fn hit(&self, suffix: &str) -> bool {
            self.paths.iter().any(|p| p.ends_with(suffix))
        }
    }

    /// Decode a form body (`a=1&b=2`, percent-free fixture codes).
    fn form(body: &[u8]) -> std::collections::BTreeMap<String, String> {
        let text = String::from_utf8_lossy(body);
        text.split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| {
                (
                    k.to_string(),
                    v.replace("%3A", ":")
                        .replace("%2F", "/")
                        .replace("%2B", "+")
                        .replace("%3D", "=")
                        .replace("%2D", "-"),
                )
            })
            .collect()
    }

    fn http_response(status: &str, content_type: &str, extra: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// One raw HTTP request per connection (like mcp.rs's serve_http);
    /// routes MCP + OAuth endpoints and records everything in `state`.
    /// The metadata points every endpoint back at THIS listener, so no
    /// request ever leaves the fixture port.
    async fn serve_oauth_fixture(
        listener: tokio::net::TcpListener,
        state: std::sync::Arc<parking_lot::Mutex<FixtureState>>,
    ) {
        let addr = listener.local_addr().unwrap();
        let auth_ep = format!("http://{addr}/authorize");
        let token_ep = format!("http://{addr}/token");
        let register_ep = format!("http://{addr}/register");
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let state = state.clone();
            let auth_ep = auth_ep.clone();
            let token_ep = token_ep.clone();
            let register_ep = register_ep.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            let text = String::from_utf8_lossy(&buf);
                            if let Some(head_end) = text.find("\r\n\r\n") {
                                let cl = text
                                    .lines()
                                    .find_map(|l| {
                                        l.to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                            .and_then(|v| v.trim().parse::<usize>().ok())
                                    })
                                    .unwrap_or(0);
                                if buf.len() >= head_end + 4 + cl {
                                    break;
                                }
                            }
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buf).into_owned();
                let Some((head, body)) = text.split_once("\r\n\r\n") else {
                    return;
                };
                let mut head_lines = head.lines();
                let request_line = head_lines.next().unwrap_or_default();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let target = parts.next().unwrap_or_default().to_string();
                let path = target.split('?').next().unwrap_or_default().to_string();
                let bearer = head_lines.find_map(|l| {
                    // case-insensitive NAME, verbatim VALUE (the value
                    // must keep its case: "Bearer <token>")
                    l.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                        .map(|(_, v)| v.trim().to_string())
                });
                let resp = {
                    let mut st = state.lock();
                    st.paths.push(path.clone());
                    if path == "/mcp" {
                        st.bearer_seen.push(bearer.clone());
                    }
                    drop(st);
                    let msg: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                    match (method.as_str(), path.as_str()) {
                        ("GET", "/.well-known/oauth-authorization-server") => http_response(
                            "200 OK",
                            "application/json",
                            "",
                            &serde_json::json!({
                                "issuer": "https://auth.test",
                                "authorization_endpoint": auth_ep,
                                "token_endpoint": token_ep,
                                "registration_endpoint": register_ep,
                                "scopes_supported": ["mcp:read", "mcp:write"],
                            })
                            .to_string(),
                        ),
                        ("POST", "/register") => {
                            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                            state.lock().registered = Some(v);
                            http_response(
                                "200 OK",
                                "application/json",
                                "",
                                &json!({"client_id": "dyn-client-1"}).to_string(),
                            )
                        }
                        ("POST", "/token") => {
                            let grant = form(body.as_bytes());
                            let ok = match grant.get("grant_type").map(String::as_str) {
                                Some("authorization_code") => {
                                    grant.get("code").map(String::as_str) == Some("test-code")
                                        && grant.contains_key("code_verifier")
                                        && grant.contains_key("redirect_uri")
                                }
                                Some("refresh_token") => {
                                    grant.get("refresh_token").map(String::as_str)
                                        == Some("rt-cached")
                                }
                                _ => false,
                            };
                            st_grant(state.clone(), grant);
                            if ok {
                                let access = match latest_grant_kind(state.clone()).as_deref() {
                                    Some("refresh_token") => "at-fresh",
                                    _ => "at-fix-1",
                                };
                                let refresh = if access == "at-fresh" {
                                    "rt-fresh"
                                } else {
                                    "rt-fix-1"
                                };
                                http_response(
                                    "200 OK",
                                    "application/json",
                                    "",
                                    &json!({
                                        "access_token": access,
                                        "refresh_token": refresh,
                                        "expires_in": 3600,
                                    })
                                    .to_string(),
                                )
                            } else {
                                http_response(
                                    "400 Bad Request",
                                    "application/json",
                                    "",
                                    &json!({"error": "invalid_grant"}).to_string(),
                                )
                            }
                        }
                        (_, "/mcp") => {
                            // any bearer passes except the poisoned
                            // probe the 401-refresh test injects
                            let authorized = bearer
                                .as_deref()
                                .map(|b| b != "Bearer at-poisoned")
                                .unwrap_or(false);
                            match msg["method"].as_str() {
                                _ if !authorized => http_response(
                                    "401 Unauthorized",
                                    "application/json",
                                    "WWW-Authenticate: Bearer realm=\"mcp\", error=\"invalid_token\"\r\n",
                                    &json!({"error": "unauthorized"}).to_string(),
                                ),
                                Some("initialize") => {
                                    let result = json!({
                                        "jsonrpc": "2.0",
                                        "id": msg["id"],
                                        "result": {
                                            "protocolVersion": msg["params"]["protocolVersion"],
                                            "capabilities": {"tools": {}},
                                            "serverInfo": {"name": "oauth-fake", "version": "0"}
                                        }
                                    });
                                    http_response(
                                        "200 OK",
                                        "application/json",
                                        "Mcp-Session-Id: sess-1\r\n",
                                        &result.to_string(),
                                    )
                                }
                                Some("notifications/initialized") => {
                                    http_response("202 Accepted", "application/json", "", "")
                                }
                                Some("tools/list") => {
                                    let result = json!({
                                        "jsonrpc": "2.0",
                                        "id": msg["id"],
                                        "result": {"tools": [
                                            {"name": "echo", "description": "echo", "inputSchema": {"type": "object"}}
                                        ]}
                                    });
                                    http_response(
                                        "200 OK",
                                        "application/json",
                                        "",
                                        &result.to_string(),
                                    )
                                }
                                Some("tools/call") => {
                                    let result = json!({
                                        "jsonrpc": "2.0",
                                        "id": msg["id"],
                                        "result": {"content": [
                                            {"type": "text",
                                             "text": format!("echo: {}", msg["params"]["arguments"]["text"])}
                                        ]}
                                    });
                                    http_response(
                                        "200 OK",
                                        "application/json",
                                        "",
                                        &result.to_string(),
                                    )
                                }
                                Some("ping") => {
                                    let result =
                                        json!({"jsonrpc": "2.0", "id": msg["id"], "result": {}});
                                    http_response(
                                        "200 OK",
                                        "application/json",
                                        "",
                                        &result.to_string(),
                                    )
                                }
                                _ => http_response("202 Accepted", "application/json", "", ""),
                            }
                        }
                        _ => http_response("404 Not Found", "text/plain", "", "nope"),
                    }
                };
                sock.write_all(&resp).await.ok();
                sock.shutdown().await.ok();
            });
        }
    }

    fn st_grant(
        state: std::sync::Arc<parking_lot::Mutex<FixtureState>>,
        grant: std::collections::BTreeMap<String, String>,
    ) {
        state.lock().grants.push(grant);
    }

    fn latest_grant_kind(
        state: std::sync::Arc<parking_lot::Mutex<FixtureState>>,
    ) -> Option<String> {
        state
            .lock()
            .grants
            .last()
            .and_then(|g| g.get("grant_type").cloned())
    }

    fn oauth_cfg(url: &str) -> crate::mcp::McpServerConfig {
        crate::mcp::McpServerConfig {
            name: "oauth-demo".into(),
            command: None,
            args: Vec::new(),
            env: Default::default(),
            url: Some(url.to_string()),
            headers: Vec::new(),
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            approval: None,
            tool_timeout_secs: None,
            output_limit: None,
            oauth: Some(true),
            oauth_client_id: None,
            oauth_scopes: vec!["mcp:read".into()],
            bearer_token_env: None,
            headers_helper: None,
        }
    }

    /// Pull the authorize URL out of the flow notes (the Note the
    /// engine surfaces), then play the browser: GET the redirect with
    /// the fixture code.
    async fn play_browser(notes: &mut mpsc::UnboundedReceiver<String>) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "no authorize URL surfaced"
            );
            let note = tokio::time::timeout(std::time::Duration::from_secs(2), notes.recv())
                .await
                .ok()
                .flatten()
                .expect("notes channel closed before the authorize URL");
            // the fixture serves plain http, so the URL starts with it
            if let Some(url) = note.lines().find(|l| l.trim_start().starts_with("http://")) {
                let url = url.trim().to_string();
                // extract state from the authorize URL and redirect to
                // the loopback callback the flow opened
                let state = url
                    .split("state=")
                    .nth(1)
                    .and_then(|s| s.split('&').next())
                    .expect("authorize URL carries state")
                    .to_string();
                let redirect = url
                    .split("redirect_uri=")
                    .nth(1)
                    .and_then(|s| s.split('&').next())
                    .expect("authorize URL carries redirect_uri");
                let redirect = redirect.replace("%3A", ":").replace("%2F", "/");
                let cb = format!("{redirect}?code=test-code&state={state}");
                let body = reqwest::get(&cb)
                    .await
                    .expect("callback GET failed")
                    .text()
                    .await
                    .unwrap_or_default();
                assert!(body.contains("authorization received"), "{body}");
                return url;
            }
        }
    }

    /// End-to-end: 401 + WWW-Authenticate → discovery → DCR →
    /// callback → code exchange → bearer works. No TTY needed (the
    /// test plays the browser through the allow_headless escape).
    #[tokio::test]
    async fn oauth_flow_completes_over_local_fixture() {
        let _g = test_support::tokens_guard();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = std::sync::Arc::new(parking_lot::Mutex::new(FixtureState::default()));
        tokio::spawn(serve_oauth_fixture(listener, state.clone()));
        let cfg = oauth_cfg(&format!("http://{addr}/mcp"));
        let (notes_tx, mut notes_rx) = mpsc::unbounded_channel();
        let hooks = FlowHooks {
            notes: Some(notes_tx),
            allow_headless: true,
            authorize_timeout: std::time::Duration::from_secs(10),
        };
        let connect = tokio::spawn(async move {
            let (client, tools) = McpClient::spawn_connect_with(&cfg, &hooks).await.unwrap();
            (client, tools)
        });
        play_browser(&mut notes_rx).await;
        let (_client, tools) = connect.await.unwrap();
        assert_eq!(tools.len(), 1, "the authorized connect lists tools");
        let st = state.lock();
        // discovery ran, registration ran (no CIMD configured)
        assert!(
            st.hit("/.well-known/oauth-authorization-server"),
            "{:?}",
            st.paths
        );
        assert!(st.hit("/register"), "DCR ran: {:?}", st.paths);
        let registered = st.registered.clone().expect("DCR body recorded");
        assert_eq!(registered["client_name"], "ka");
        assert!(
            registered["redirect_uris"][0]
                .as_str()
                .unwrap()
                .starts_with("http://127.0.0.1:"),
            "loopback redirect registered: {registered}"
        );
        // the code exchange validated the fixture code + PKCE verifier
        assert!(
            st.grants
                .iter()
                .any(
                    |g| g.get("grant_type").map(String::as_str) == Some("authorization_code")
                        && g.get("code").map(String::as_str) == Some("test-code")
                        && g.contains_key("code_verifier")
                ),
            "grants: {:?}",
            st.grants
        );
        // the retried handshake carried the fresh bearer
        assert!(
            st.bearer_seen
                .iter()
                .any(|b| b.as_deref() == Some("Bearer at-fix-1")),
            "bearer seen: {:?}",
            st.bearer_seen
        );
        drop(st);
        // the token set was cached for the next run
        let key = server_key(&oauth_cfg(&format!("http://{addr}/mcp")));
        let cached = load_tokens(&key).expect("tokens cached");
        assert_eq!(cached.access_token, "at-fix-1");
        assert_eq!(cached.refresh_token.as_deref(), Some("rt-fix-1"));
        assert_eq!(cached.client_id.as_deref(), Some("dyn-client-1"));
    }

    /// Cached-but-expired tokens refresh headlessly (no authorize,
    /// no registration): the refresh grant rotates and re-caches.
    #[tokio::test]
    async fn expired_cache_refreshes_via_refresh_grant() {
        let _g = test_support::tokens_guard();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = std::sync::Arc::new(parking_lot::Mutex::new(FixtureState::default()));
        tokio::spawn(serve_oauth_fixture(listener, state.clone()));
        let cfg = oauth_cfg(&format!("http://{addr}/mcp"));
        let key = server_key(&cfg);
        store_tokens(
            &key,
            &TokenSet {
                access_token: "at-stale".into(),
                refresh_token: Some("rt-cached".into()),
                client_id: Some("cimd-client".into()),
                expires_at: Some(now_epoch_secs().saturating_sub(10)),
            },
        );
        let (mut client, tools) = McpClient::spawn_connect(&cfg).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert!(client.alive().await);
        let st = state.lock();
        // the refresh grant ran with the CACHED client id
        assert!(
            st.grants
                .iter()
                .any(
                    |g| g.get("grant_type").map(String::as_str) == Some("refresh_token")
                        && g.get("refresh_token").map(String::as_str) == Some("rt-cached")
                        && g.get("client_id").map(String::as_str) == Some("cimd-client")
                ),
            "grants: {:?}",
            st.grants
        );
        // no interactive flow ran
        assert!(!st.hit("/register"), "no DCR on refresh: {:?}", st.paths);
        // the retried handshake carried the refreshed bearer
        assert!(
            st.bearer_seen
                .iter()
                .any(|b| b.as_deref() == Some("Bearer at-fresh")),
            "bearer seen: {:?}",
            st.bearer_seen
        );
        drop(st);
        let cached = load_tokens(&key).expect("refreshed tokens re-cached");
        assert_eq!(cached.access_token, "at-fresh");
        assert_eq!(cached.client_id.as_deref(), Some("cimd-client"));
    }

    /// bearer_token_env wins over everything: no OAuth traffic at all,
    /// the env token rides as the bearer. (HOME exists in every test
    /// environment — set_var is unsafe in edition 2024 and forbidden.)
    #[tokio::test]
    async fn bearer_env_disables_oauth_entirely() {
        let _g = test_support::tokens_guard();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = std::sync::Arc::new(parking_lot::Mutex::new(FixtureState::default()));
        tokio::spawn(serve_oauth_fixture(listener, state.clone()));
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/katest".into());
        let mut cfg = oauth_cfg(&format!("http://{addr}/mcp"));
        cfg.bearer_token_env = Some("HOME".into());
        assert!(
            !cfg.oauth_enabled(),
            "bearer_token_env must switch OAuth off"
        );
        let (mut client, tools) = McpClient::spawn_connect(&cfg).await.unwrap();
        assert_eq!(tools.len(), 1);
        let text = client
            .call_tool("echo", json!({"text": "hi"}))
            .await
            .unwrap();
        let _ = text;
        let st = state.lock();
        assert!(
            !st.hit("/.well-known/oauth-authorization-server"),
            "no discovery under bearer_token_env: {:?}",
            st.paths
        );
        assert!(!st.hit("/register"), "no DCR under bearer_token_env");
        assert!(
            st.bearer_seen
                .iter()
                .all(|b| b.as_deref() == Some(&format!("Bearer {home}"))),
            "env bearer rides: {:?}",
            st.bearer_seen
        );
    }

    /// Without cached tokens and without a TTY (allow_headless false)
    /// the flow fails up front with the `ka mcp login` instruction —
    /// never a silent hang.
    #[tokio::test]
    async fn headless_without_cache_fails_with_login_instruction() {
        let _g = test_support::tokens_guard();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = std::sync::Arc::new(parking_lot::Mutex::new(FixtureState::default()));
        tokio::spawn(serve_oauth_fixture(listener, state.clone()));
        let cfg = oauth_cfg(&format!("http://{addr}/mcp"));
        let err = McpClient::spawn_connect(&cfg).await.unwrap_err();
        assert!(err.contains("ka mcp login oauth-demo"), "{err}");
        assert!(err.contains("bearer_token_env"), "{err}");
    }

    /// A mid-session expiry refreshes ONCE inside the transport
    /// (401 → refresh grant → retry) and then fails loud if the
    /// server still refuses.
    #[tokio::test]
    async fn transport_refreshes_once_on_401() {
        let _g = test_support::tokens_guard();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = std::sync::Arc::new(parking_lot::Mutex::new(FixtureState::default()));
        tokio::spawn(serve_oauth_fixture(listener, state.clone()));
        let cfg = oauth_cfg(&format!("http://{addr}/mcp"));
        let key = server_key(&cfg);
        store_tokens(
            &key,
            &TokenSet {
                access_token: "at-fresh".into(),
                refresh_token: Some("rt-cached".into()),
                client_id: Some("cimd-client".into()),
                expires_at: None, // reads unexpired; the server 401s anyway
            },
        );
        let (mut client, _tools) = McpClient::spawn_connect(&cfg).await.unwrap();
        // the fixture accepts `Bearer at-fresh` — this call succeeds
        // without any refresh
        let out = client.call_tool("echo", json!({"text": "x"})).await;
        assert!(out.is_ok(), "{out:?}");
        // now poison the in-transport bearer: the next call 401s once,
        // refreshes via the grant, and retries successfully
        if let Some(c) = client_cell_bearer(&mut client) {
            *c = "at-poisoned".to_string();
        }
        let out = client
            .call_tool("echo", json!({"text": "after-refresh"}))
            .await
            .unwrap();
        assert_eq!(out, "echo: \"after-refresh\"");
        let st = state.lock();
        assert!(
            st.grants
                .iter()
                .any(|g| g.get("grant_type").map(String::as_str) == Some("refresh_token")),
            "the 401 triggered one refresh: {:?}",
            st.grants
        );
        drop(st);
        let cached = load_tokens(&key).expect("refresh re-cached");
        assert_eq!(cached.access_token, "at-fresh");
    }

    /// Reach the transport's bearer for the refresh test (test-only
    /// poke through the client).
    fn client_cell_bearer(client: &mut McpClient) -> Option<&mut String> {
        client.test_bearer_mut()
    }
}
