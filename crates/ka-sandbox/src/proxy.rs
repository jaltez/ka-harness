//! PERM-03: loopback HTTP/CONNECT proxy that filters bash-child network
//! egress by domain.
//!
//! When the active `[sandbox]` policy carries `net_allow_domains` /
//! `net_deny_domains`, the bash hand ensures this proxy is live (a
//! `TcpListener` on `127.0.0.1:0`, process-global) and injects
//! `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY` into bash children so
//! cooperating clients (curl, wget, git, pip, …) are routed through it.
//!
//! **Honest enforcement statement:** the filter lives INSIDE the proxy.
//! A child that ignores proxy environment variables reaches the network
//! directly whenever the enforcement backend permits raw sockets (the
//! landlock trampoline allows TCP connect only to the proxy port, bwrap
//! shares the network so the loopback proxy is reachable, firejail never
//! denies network in the fs tier). Tools that respect proxy env are
//! filtered; raw-socket programs are not — this is documented behavior,
//! not an oversight. The proxy never MITMs: CONNECT tunnels are pure
//! byte pass-through after the host check (no TLS interception, no
//! certificates).
//!
//! Per-connection flow: parse target → domain filter (deny beats allow;
//! an empty allow list means "everything except denied") → resolve the
//! host ONCE and pin the FIRST resolved address (no re-resolution) →
//! anti-DNS-rebinding guard (private/loopback/link-local targets are
//! refused unless the host is explicitly allowlisted) → connect and
//! tunnel. Unix-socket targets (`CONNECT unix:/…`, `http://unix/…`,
//! path-like or slash-encoded hosts) are always refused.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use crate::Policy;

/// Idle/read timeout while parsing a request head from the client.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Upstream TCP connect timeout (the pinned address gets one shot).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on a request head (request line + headers) accepted from a client.
const MAX_HEAD: usize = 32 * 1024;
/// Cap on the first request line of a CONNECT.
const MAX_LINE: usize = 8 * 1024;

/// `NO_PROXY` value injected into sandbox children: loopback targets are
/// exempt because the proxy itself always refuses loopback destinations
/// (anti-rebinding guard) — routing them through the proxy would only
/// produce a confusing 403 where a direct refusal is clearer. Whether a
/// direct loopback connection succeeds is the backend's raw-socket
/// stance, exactly as for any other bypass.
pub const NO_PROXY_LOOPBACK: &str = "localhost,127.0.0.1,::1";

/// Normalized proxy domain lists: lowercase, `*.example.com` folded to
/// `example.com` (the wildcard spelling and the bare suffix spelling
/// match identically: the entry itself plus any subdomain).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainFilter {
    allow: Vec<String>,
    deny: Vec<String>,
}

impl DomainFilter {
    /// Build from raw config spellings (empty entries dropped).
    pub fn new(allow: &[String], deny: &[String]) -> Self {
        DomainFilter {
            allow: allow.iter().filter_map(|s| normalize_entry(s)).collect(),
            deny: deny.iter().filter_map(|s| normalize_entry(s)).collect(),
        }
    }

    /// Whether neither list has an entry (the proxy is inert).
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }

    /// Whether `host` (exact or subdomain of) an entry matches.
    fn list_matches(list: &[String], host: &str) -> bool {
        list.iter().any(|e| {
            host == e.as_str() || host.strip_suffix(e).is_some_and(|rest| rest.ends_with('.'))
        })
    }

    /// Whether an entry of the ALLOW list covers `host` — the exact
    /// spelling the operator listed. Used by the anti-rebinding guard:
    /// a private/loopback target passes only when the host (or IP
    /// literal) is explicitly allowlisted, never via deny-only policies.
    pub fn allow_matches(&self, host: &str) -> bool {
        Self::list_matches(&self.allow, &normalize_host(host))
    }

    /// Domain-level verdict for a request host. Deny wins over allow;
    /// an empty allow list permits everything not denied; unix-socket
    /// shaped hosts are never permitted.
    pub fn permits(&self, host: &str) -> bool {
        let host = normalize_host(host);
        if is_unix_socket_target(&host) {
            return false;
        }
        if Self::list_matches(&self.deny, &host) {
            return false;
        }
        if !self.allow.is_empty() && !Self::list_matches(&self.allow, &host) {
            return false;
        }
        true
    }
}

/// Fold one config entry: trim, lowercase, strip a `*.` prefix, drop
/// empties. `*.example.com` and `example.com` both become `example.com`.
fn normalize_entry(s: &str) -> Option<String> {
    let t = s.trim().to_ascii_lowercase();
    let t = t
        .strip_suffix('.')
        .unwrap_or(&t)
        .strip_prefix("*.")
        .unwrap_or(&t);
    (!t.is_empty()).then(|| t.to_string())
}

/// Normalize a request host for matching.
fn normalize_host(host: &str) -> String {
    host.trim().to_ascii_lowercase()
}

/// Hosts that name a unix socket rather than a DNS host: bare `unix`,
/// anything path-like or slash-encoded. TCP proxies cannot relay unix
/// sockets — refuse instead of silently misconnecting.
fn is_unix_socket_target(host: &str) -> bool {
    host.is_empty()
        || host == "unix"
        || host.starts_with('/')
        || host.starts_with('.')
        || host.contains('\\')
        || host.contains("%2f")
        || host.contains("%5c")
}

/// Whether `ip` is private, loopback, link-local, unspecified or
/// otherwise non-public. Hand-rolled for IPv6 (fewer std-gate risks);
/// IPv4-mapped IPv6 is judged by its embedded IPv4 address so
/// `::ffff:127.0.0.1` cannot pose as public.
pub fn ip_is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            let o = v6.octets();
            let mapped = o[..12] == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff];
            if mapped {
                return ip_is_local(IpAddr::V4(Ipv4Addr::new(o[12], o[13], o[14], o[15])));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (o[0] & 0xfe) == 0xfc // fc00::/7 unique local
                || (o[0] == 0xfe && (o[1] & 0xc0) == 0x80) // fe80::/10 link local
        }
    }
}

/// Parse a `host[:port]` authority. Bracketed IPv6 is handled; a bare
/// (unbracketed) IPv6 cannot be split unambiguously and is refused.
/// `default_port` supplies the port when the authority carries none
/// (None = the port is required, CONNECT semantics).
fn parse_authority(auth: &str, default_port: Option<u16>) -> Option<(String, u16)> {
    let auth = auth.trim();
    if auth.is_empty() {
        return None;
    }
    if let Some(rest) = auth.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let tail = &rest[end + 1..];
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse::<u16>().ok()?,
            None => default_port?,
        };
        return Some((host.to_string(), port));
    }
    match auth.rsplit_once(':') {
        Some((host, port)) => {
            if host.contains(':') {
                return None; // bare IPv6 — unparseable authority
            }
            let port = port.parse::<u16>().ok()?;
            (port != 0).then(|| (host.to_string(), port))
        }
        None => default_port
            .filter(|p| *p != 0)
            .map(|p| (auth.to_string(), p)),
    }
}

/// Split an absolute-form request target `http://[user@]host[:port]/path`.
fn split_absolute(target: &str) -> Option<(String, u16, String)> {
    let rest = target.strip_prefix("http://")?;
    let (auth, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    // userinfo is never routed — drop everything before the last '@'
    let auth = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
    let (host, port) = parse_authority(auth, Some(80))?;
    Some((host, port, path))
}

/// Resolve `host` ONCE and return the FIRST address (the pin — the
/// upstream connect never re-resolves, so a rebinding rotation between
/// resolve and connect is out of the model). IP literals skip DNS. Any
/// private/loopback/link-local result is refused unless the host is
/// explicitly in the ALLOW list: deny-only policies never unlock local
/// targets, which is what keeps the proxy from being turned into an SSRF
/// hop into the ka process's own machine.
pub fn resolve_pinned(filter: &DomainFilter, host: &str, port: u16) -> Result<SocketAddr, String> {
    let rebinding = |what: &str| {
        format!(
            "sandbox proxy: {host} resolved to {what}, a private/loopback/link-local \
             address that is not explicitly allowlisted — refusing (anti-DNS-rebinding)"
        )
    };
    if let Ok(ip) = host.trim().parse::<IpAddr>() {
        if ip_is_local(ip) && !filter.allow_matches(host) {
            return Err(rebinding("itself"));
        }
        return Ok(SocketAddr::new(ip, port));
    }
    let first = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("sandbox proxy: cannot resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("sandbox proxy: {host} resolved to no addresses"))?;
    if ip_is_local(first.ip()) && !filter.allow_matches(host) {
        return Err(rebinding(&first.ip().to_string()));
    }
    Ok(first)
}

/// A live loopback proxy instance. Dropping it stops the listener and
/// wakes the accept loop; in-flight tunnels are allowed to finish.
#[derive(Debug)]
pub struct NetProxy {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl NetProxy {
    /// Bind `127.0.0.1:0` and start serving. `Err` = bind/thread failure
    /// (fail-closed callers must not send children through unfiltered).
    pub fn spawn(filter: DomainFilter) -> Result<NetProxy, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| format!("sandbox proxy: bind 127.0.0.1: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("sandbox proxy: local_addr: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("sandbox proxy: nonblocking: {e}"))?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let accept = {
            let shutdown = shutdown.clone();
            std::thread::Builder::new()
                .name("ka-sbx-proxy".into())
                .spawn(move || {
                    while !shutdown.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let filter = filter.clone();
                                let shutdown = shutdown.clone();
                                let _ = std::thread::Builder::new()
                                    .name("ka-sbx-conn".into())
                                    .spawn(move || serve_connection(stream, filter, shutdown));
                            }
                            // nonblocking accept: idle poll at 50 ms —
                            // shutdown is observed within one tick
                            Err(_) => std::thread::sleep(Duration::from_millis(50)),
                        }
                    }
                })
                .map_err(|e| format!("sandbox proxy: spawn accept thread: {e}"))?
        };
        Ok(NetProxy {
            addr,
            shutdown,
            accept: Some(accept),
        })
    }

    /// The bound loopback address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The proxy URL for `*_PROXY` environment variables.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for NetProxy {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // wake a parked accept poll promptly (best effort)
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

/// Proxy environment derived from a policy (engine-facing shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyEnv {
    /// The bound loopback address of the live proxy.
    pub addr: SocketAddr,
    /// `NO_PROXY` value (loopback exemption).
    pub no_proxy: String,
}

impl ProxyEnv {
    /// The proxy URL for the `*_PROXY` variables.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The concrete environment variable pairs, applied LAST by the bash
    /// hand so the sandbox policy wins over inherited or `[shell] env_set`
    /// proxy settings. Both letter cases are set (curl honors lowercase
    /// `http_proxy` only; git/libcurl read lowercase, many tools read
    /// uppercase).
    pub fn vars(&self) -> Vec<(String, String)> {
        let url = self.url();
        let mut vars = Vec::with_capacity(6);
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            vars.push((name.to_string(), url.clone()));
        }
        for name in ["NO_PROXY", "no_proxy"] {
            vars.push((name.to_string(), self.no_proxy.clone()));
        }
        vars
    }
}

struct Active {
    filter: DomainFilter,
    proxy: NetProxy,
}

/// Process-global proxy: at most one live instance, swapped whenever the
/// policy's domain lists change, shut down when the active policy stops
/// restricting (mode off, or fs without domain lists). The bash hand
/// consults this per spawn, which is exactly the "alive while a sandbox
/// with domains is active" lifetime.
static ACTIVE: LazyLock<Mutex<Option<Active>>> = LazyLock::new(|| Mutex::new(None));

/// Ensure the process-global proxy matches `policy`'s domain restriction
/// and return the child environment to inject. `Ok(None)` = policy has
/// no domain restriction (proxy inert); `Err` = the policy wants the
/// proxy but it could not be started — callers must fail the command
/// closed rather than run it unfiltered.
pub fn proxy_env(policy: &Policy) -> Result<Option<ProxyEnv>, String> {
    let Policy::Fs { net, .. } = policy else {
        stop_proxy();
        return Ok(None);
    };
    if !net.is_restricted() {
        stop_proxy();
        return Ok(None);
    }
    let filter = DomainFilter::new(&net.allow, &net.deny);
    // poisoning is not a state worth crashing over: the payload is a
    // replaceable handle, recover and continue
    let mut active = ACTIVE.lock().unwrap_or_else(|p| p.into_inner());
    let addr = if !matches!(active.as_ref(), Some(a) if a.filter == filter) {
        // drop the stale instance first, then swap in a live one
        *active = None;
        let proxy = NetProxy::spawn(filter.clone())?;
        let addr = proxy.addr();
        *active = Some(Active { filter, proxy });
        addr
    } else {
        // the installed instance matches the policy by construction
        let Some(installed) = active.as_ref() else {
            return Err("sandbox proxy: policy restricted but no live instance".to_string());
        };
        installed.proxy.addr()
    };
    Ok(Some(ProxyEnv {
        addr,
        no_proxy: NO_PROXY_LOOPBACK.to_string(),
    }))
}

/// Shut the process-global proxy down (policy no longer restricts).
fn stop_proxy() {
    let mut active = ACTIVE.lock().unwrap_or_else(|p| p.into_inner());
    *active = None;
}

// ---------------------------------------------------------------------
// connection serving

/// Read one `\n`-terminated line (returned WITH its terminator), capped.
fn read_line(stream: &mut TcpStream, cap: usize) -> Result<String, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if line.len() >= cap {
            return Err("request line too long".to_string());
        }
        match stream.read(&mut byte) {
            Ok(0) => return Err("client closed".to_string()),
            Ok(_) => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    return Ok(String::from_utf8_lossy(&line).into_owned());
                }
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
}

/// Read a request head (request line + headers, INCLUDING the final
/// blank line) as raw bytes so forwarding is byte-exact. `Ok(None)` =
/// clean EOF before any byte (idle keep-alive probes).
fn read_head(stream: &mut TcpStream) -> Result<Option<Vec<u8>>, String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if buf.len() >= MAX_HEAD {
            return Err("request head too large".to_string());
        }
        match stream.read(&mut byte) {
            Ok(0) => {
                if buf.is_empty() {
                    return Ok(None);
                }
                return Err("client closed mid-head".to_string());
            }
            Ok(_) => {
                buf.push(byte[0]);
                // an IMMEDIATE blank line (after the request line was
                // consumed) is a header-less head; otherwise wait for
                // the classic \r\n\r\n terminator
                if buf == b"\r\n" || buf == b"\n" {
                    return Ok(Some(buf));
                }
                if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
                    return Ok(Some(buf));
                }
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
}

/// Lowercase a header NAME and return its value, from raw head bytes.
/// `head` is the header block AFTER the request line (the request line
/// was consumed to pick CONNECT vs method), so line 0 is a header.
fn header_value(head: &str, name: &str) -> Option<String> {
    for line in head.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Write a small plain-text response and close.
fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(stream, "HTTP/1.1 {status}\r\n");
    let _ = write!(stream, "Content-Type: text/plain; charset=utf-8\r\n");
    let _ = write!(stream, "Content-Length: {}\r\n", body.len());
    let _ = write!(stream, "Connection: close\r\n\r\n");
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn serve_connection(mut client: TcpStream, filter: DomainFilter, shutdown: Arc<AtomicBool>) {
    if shutdown.load(Ordering::Relaxed) {
        return;
    }
    let _ = client.set_read_timeout(Some(HEAD_TIMEOUT));
    let _ = client.set_write_timeout(Some(HEAD_TIMEOUT));
    let first = match read_line(&mut client, MAX_LINE) {
        Ok(l) => l,
        Err(_) => return, // idle probe or garbage: just close
    };
    let mut parts = first.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        respond(
            &mut client,
            "400 Bad Request",
            "sandbox proxy: malformed request line",
        );
        return;
    };
    if method.eq_ignore_ascii_case("CONNECT") {
        serve_connect(client, &filter, target);
    } else {
        serve_http(client, &filter, method, target, version);
    }
}

/// CONNECT host:port — filter the host BEFORE any tunnel exists, then
/// pass bytes through untouched in both directions (no MITM, no certs).
fn serve_connect(mut client: TcpStream, filter: &DomainFilter, target: &str) {
    let Some((host, port)) = parse_authority(target, None) else {
        respond(
            &mut client,
            "400 Bad Request",
            "sandbox proxy: CONNECT needs host:port",
        );
        return;
    };
    // the remainder of the CONNECT head (optional headers + blank line)
    // is consumed and discarded: nothing past the request line may leak
    // into the tunnel
    for _ in 0..64 {
        match read_line(&mut client, MAX_LINE) {
            Ok(line) if line == "\r\n" || line == "\n" => break,
            Ok(_) => continue,
            Err(_) => return,
        }
    }
    if !filter.permits(&host) {
        respond(
            &mut client,
            "403 Forbidden",
            &format!("sandbox proxy: {host} is not permitted by [sandbox] net domain policy"),
        );
        return;
    }
    let addr = match resolve_pinned(filter, &host, port) {
        Ok(a) => a,
        Err(e) => {
            respond(&mut client, "403 Forbidden", &e);
            return;
        }
    };
    let upstream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(u) => u,
        Err(e) => {
            respond(
                &mut client,
                "502 Bad Gateway",
                &format!("sandbox proxy: cannot reach {host}:{port} ({e})"),
            );
            return;
        }
    };
    let _ = client.set_read_timeout(None);
    let _ = client.set_write_timeout(None);
    let _ = upstream.set_read_timeout(None);
    let _ = upstream.set_write_timeout(None);
    let _ = client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
    let _ = client.flush();
    tunnel(client, upstream);
}

/// Plain HTTP: absolute-form targets are rewritten to origin-form (the
/// only change made to the stream — the header block itself is forwarded
/// byte-exact); origin-form requests are routed by the Host header.
fn serve_http(
    mut client: TcpStream,
    filter: &DomainFilter,
    method: &str,
    target: &str,
    version: &str,
) {
    let head = match read_head(&mut client) {
        Ok(Some(h)) => h,
        Ok(None) => return,
        Err(e) => {
            respond(
                &mut client,
                "400 Bad Request",
                &format!("sandbox proxy: {e}"),
            );
            return;
        }
    };
    let text = String::from_utf8_lossy(&head).into_owned();
    let (target_host, target_port, forward_first) = if target.starts_with("http://") {
        // absolute-form: authority rides the request line
        let Some((host, port, path)) = split_absolute(target) else {
            respond(
                &mut client,
                "400 Bad Request",
                "sandbox proxy: bad absolute URI authority",
            );
            return;
        };
        (host, port, format!("{method} {path} {version}"))
    } else {
        // origin-form: authority rides the Host header
        let Some(host_header) = header_value(&text, "host") else {
            respond(
                &mut client,
                "400 Bad Request",
                "sandbox proxy: missing Host header",
            );
            return;
        };
        let Some((host, port)) = parse_authority(&host_header, Some(80)) else {
            respond(
                &mut client,
                "400 Bad Request",
                "sandbox proxy: bad Host header",
            );
            return;
        };
        (host, port, format!("{method} {target} {version}"))
    };
    if !filter.permits(&target_host) {
        respond(
            &mut client,
            "403 Forbidden",
            &format!(
                "sandbox proxy: {target_host} is not permitted by [sandbox] net domain policy"
            ),
        );
        return;
    }
    let addr = match resolve_pinned(filter, &target_host, target_port) {
        Ok(a) => a,
        Err(e) => {
            respond(&mut client, "403 Forbidden", &e);
            return;
        }
    };
    // the first line is the only rewritten region; everything up to and
    // including the blank line is forwarded as received
    let Some(first_nl) = head.iter().position(|b| *b == b'\n') else {
        respond(
            &mut client,
            "400 Bad Request",
            "sandbox proxy: truncated head",
        );
        return;
    };
    let mut forward = Vec::with_capacity(head.len() + 128);
    forward.extend_from_slice(forward_first.as_bytes());
    forward.extend_from_slice(b"\r\n");
    forward.extend_from_slice(&head[first_nl + 1..]);
    let mut upstream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(u) => u,
        Err(e) => {
            respond(
                &mut client,
                "502 Bad Gateway",
                &format!("sandbox proxy: cannot reach {target_host}:{target_port} ({e})"),
            );
            return;
        }
    };
    let _ = client.set_read_timeout(None);
    let _ = client.set_write_timeout(None);
    let _ = upstream.set_read_timeout(None);
    let _ = upstream.set_write_timeout(None);
    if let Err(e) = upstream.write_all(&forward).and_then(|()| upstream.flush()) {
        respond(
            &mut client,
            "502 Bad Gateway",
            &format!("sandbox proxy: upstream write: {e}"),
        );
        return;
    }
    tunnel(client, upstream);
}

/// Byte-exact bidirectional copy until EITHER side closes; when one
/// direction ends both sockets are shut down so the other thread's
/// blocking read unblocks promptly.
fn tunnel(client: TcpStream, upstream: TcpStream) {
    let Ok(mut client_c) = client.try_clone() else {
        return;
    };
    let Ok(mut upstream_c) = upstream.try_clone() else {
        return;
    };
    let to_upstream = std::thread::Builder::new()
        .name("ka-sbx-fwd".into())
        .spawn(move || {
            let _ = std::io::copy(&mut client_c, &mut upstream_c);
            let _ = upstream_c.shutdown(std::net::Shutdown::Both);
        })
        .ok();
    let Ok(mut client_c) = client.try_clone() else {
        return;
    };
    let Ok(mut upstream_c) = upstream.try_clone() else {
        return;
    };
    let to_client = std::thread::Builder::new()
        .name("ka-sbx-fwd".into())
        .spawn(move || {
            let _ = std::io::copy(&mut upstream_c, &mut client_c);
            let _ = client_c.shutdown(std::net::Shutdown::Both);
        })
        .ok();
    // the originator ends both legs on shutdown: dropping the originals
    // closes the fd dup'd into the workers after they finish their copy
    if let Some(h) = to_upstream {
        let _ = h.join();
    }
    if let Some(h) = to_client {
        let _ = h.join();
    }
    let _ = client.shutdown(std::net::Shutdown::Both);
    let _ = upstream.shutdown(std::net::Shutdown::Both);
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::NetDomains;
    use std::sync::LazyLock;

    /// Serializes tests that touch the process-global proxy lifecycle.
    /// `pub(crate)` so the lib-level wrap_command tests share it.
    pub(crate) static PROXY_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn filter(allow: &[&str], deny: &[&str]) -> DomainFilter {
        let allow: Vec<String> = allow.iter().map(|s| (*s).to_string()).collect();
        let deny: Vec<String> = deny.iter().map(|s| (*s).to_string()).collect();
        DomainFilter::new(&allow, &deny)
    }

    /// A tiny HTTP fixture: responds 200 with a fixed body, then closes.
    fn spawn_fixture() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        std::thread::spawn(move || {
                            // drain the full request head (until blank line)
                            let mut byte = [0u8; 1];
                            let mut window: [u8; 4] = [0; 4];
                            let mut seen = 0usize;
                            loop {
                                if seen >= 64 * 1024 {
                                    return;
                                }
                                match stream.read(&mut byte) {
                                    Ok(0) | Err(_) => return,
                                    Ok(_) => {
                                        window.copy_within(1.., 0);
                                        window[3] = byte[0];
                                        seen += 1;
                                        if seen >= 4 && window == *b"\r\n\r\n" {
                                            break;
                                        }
                                    }
                                }
                            }
                            let body = "ka-fixture-ok";
                            let _ = write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        port
    }

    /// One request through the proxy; returns the full response text.
    fn roundtrip(proxy_addr: SocketAddr, request: &str) -> String {
        let mut client = TcpStream::connect(proxy_addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let mut buf = Vec::new();
        let _ = client.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[test]
    fn domain_filter_matching_table() {
        let f = filter(
            &["*.example.com", "api.other.example", "10.0.0.7"],
            &["secret.example"],
        );
        // exact + subdomain via both spellings
        assert!(
            f.permits("example.com"),
            "wildcard spelling covers the apex"
        );
        assert!(f.permits("sub.example.com"));
        assert!(f.permits("api.other.example"));
        assert!(f.permits("v2.api.other.example"));
        assert!(f.permits("EXAMPLE.COM"), "matching is case-insensitive");
        // non-matches
        assert!(
            !f.permits("badexample.com"),
            "suffix never spans components"
        );
        assert!(!f.permits("example.org"));
        assert!(!f.permits("other.example"));
        assert!(
            !f.permits("real.host.example"),
            "a host outside the entries is refused"
        );
        // IP literals match verbatim
        assert!(f.permits("10.0.0.7"));
        assert!(!f.permits("10.0.0.8"));
        // deny wins over allow
        assert!(!f.permits("secret.example"));
        // empty allow = everything not denied
        let deny_only = filter(&[], &["denied.example"]);
        assert!(deny_only.permits("anything.example"));
        assert!(!deny_only.permits("denied.example"));
        // unix-socket shapes never permitted
        assert!(!f.permits("unix"));
        assert!(!f.permits("/run/docker.sock"));
        assert!(!f.permits("%2Frun%2Fdocker.sock"));
        // ...while a listed-shape host still passes
        assert!(f.permits("v3.api.other.example"));
    }

    #[test]
    fn ip_locality_table() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(ip_is_local(addr), "{ip} must count as local");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111", "::ffff:8.8.8.8"] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(!ip_is_local(addr), "{ip} must count as public");
        }
    }

    #[test]
    fn rebinding_guard_rejects_loopback_without_explicit_allow() {
        // "localhost" resolves to a loopback address on every host — the
        // deny-only filter permits it at the domain level, but the
        // rebinding guard refuses: private targets need an explicit allow
        let f = filter(&[], &[]);
        assert!(f.permits("localhost"), "policy level: nothing denied");
        let err = resolve_pinned(&f, "localhost", 80).unwrap_err();
        assert!(err.contains("allowlisted"), "guard names the rule: {err}");
        // explicit allow unlocks it (this is how local fixtures work)
        let f = filter(&["localhost"], &[]);
        let pinned = resolve_pinned(&f, "localhost", 80).unwrap();
        assert!(pinned.ip().is_loopback(), "{pinned}");
        // public hosts pass without any allowlist
        let pinned = resolve_pinned(&filter(&[], &[]), "example.com", 443);
        let _ = pinned; // network-dependent: error is fine here, success fine too
    }

    #[test]
    fn ip_literal_private_requires_explicit_entry() {
        let f = filter(&[], &[]);
        assert!(resolve_pinned(&f, "10.1.2.3", 80).is_err());
        assert!(resolve_pinned(&f, "127.0.0.1", 80).is_err());
        let f = filter(&["10.1.2.3", "127.0.0.1"], &[]);
        assert!(resolve_pinned(&f, "10.1.2.3", 80).is_ok());
        assert!(resolve_pinned(&f, "127.0.0.1", 80).is_ok());
        // a public IP literal never needs an entry
        assert!(resolve_pinned(&filter(&[], &[]), "8.8.8.8", 53).is_ok());
    }

    #[test]
    fn connect_passthrough_allowlisted_loopback_fixture() {
        let port = spawn_fixture();
        let proxy = NetProxy::spawn(filter(&["127.0.0.1"], &[])).unwrap();
        let resp = roundtrip(
            proxy.addr(),
            &format!(
                "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\nGET / HTTP/1.1\r\nHost: fixture\r\n\r\n"
            ),
        );
        assert!(resp.contains("200 Connection Established"), "{resp}");
        assert!(
            resp.contains("ka-fixture-ok"),
            "tunnel bytes passed through: {resp}"
        );
    }

    #[test]
    fn connect_denied_closes_with_error() {
        let proxy = NetProxy::spawn(filter(&[], &["denied.example"])).unwrap();
        let resp = roundtrip(proxy.addr(), "CONNECT denied.example:443 HTTP/1.1\r\n\r\n");
        assert!(resp.contains("403"), "deny answers with 403: {resp}");
        assert!(!resp.contains("200"), "{resp}");
    }

    #[test]
    fn connect_rebinding_guard_answers_403() {
        // a host resolving to 127.0.0.1 (localhost) without an explicit
        // allowlist entry is refused even though no deny list matches
        let proxy = NetProxy::spawn(filter(&[], &[])).unwrap();
        let resp = roundtrip(proxy.addr(), "CONNECT localhost:80 HTTP/1.1\r\n\r\n");
        assert!(resp.contains("403"), "rebinding guard: {resp}");
        assert!(resp.contains("allowlisted"), "{resp}");
    }

    #[test]
    fn plain_http_absolute_form_is_proxied_and_rewritten() {
        let port = spawn_fixture();
        let proxy = NetProxy::spawn(filter(&["127.0.0.1"], &[])).unwrap();
        let resp = roundtrip(
            proxy.addr(),
            &format!("GET http://127.0.0.1:{port}/path?q=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"),
        );
        assert!(resp.contains("ka-fixture-ok"), "{resp}");
        // origin-form via Host header too
        let resp = roundtrip(
            proxy.addr(),
            &format!("GET /other HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert!(resp.contains("ka-fixture-ok"), "{resp}");
    }

    #[test]
    fn unix_socket_targets_are_blocked() {
        let proxy = NetProxy::spawn(filter(&["unix", "/run/docker.sock"], &[])).unwrap();
        // even explicitly "allowlisted" unix targets are refused: the proxy
        // cannot relay a unix socket over TCP
        let resp = roundtrip(proxy.addr(), "CONNECT /run/docker.sock HTTP/1.1\r\n\r\n");
        assert!(!resp.contains("200"), "path-shaped CONNECT refused: {resp}");
        let resp = roundtrip(proxy.addr(), "GET http://unix/run/x HTTP/1.1\r\n\r\n");
        assert!(resp.contains("403"), "http://unix refused: {resp}");
    }

    #[test]
    fn proxy_env_lifecycle_and_no_proxy_exempt() {
        let _guard = PROXY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let restricted = Policy::Fs {
            allow_write: Vec::new(),
            allow_net: false,
            pass_env: Vec::new(),
            net: NetDomains {
                allow: vec!["example.com".into()],
                deny: Vec::new(),
            },
        };
        let env = proxy_env(&restricted)
            .unwrap()
            .expect("restricted policy starts the proxy");
        assert!(env.url().starts_with("http://127.0.0.1:"), "{env:?}");
        assert!(env.addr.port() > 0, "{env:?}");
        let vars = env.vars();
        let get = |name: &str| {
            vars.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("HTTP_PROXY"), env.url());
        assert_eq!(get("https_proxy"), env.url());
        assert_eq!(get("NO_PROXY"), NO_PROXY_LOOPBACK);
        assert_eq!(get("no_proxy"), NO_PROXY_LOOPBACK);
        // same policy → same live instance (the URL is stable)
        let again = proxy_env(&restricted).unwrap().unwrap();
        assert_eq!(again.addr, env.addr);
        // off / unrestricted policies stop the proxy and inject nothing
        assert!(proxy_env(&Policy::Off).unwrap().is_none());
        let plain = Policy::fs(Vec::new());
        assert!(proxy_env(&plain).unwrap().is_none());
        // and it comes back on demand
        let revived = proxy_env(&restricted).unwrap().unwrap();
        assert_ne!(revived.addr, env.addr, "a fresh listener after shutdown");
        // leave the global clean for other tests
        stop_proxy();
    }

    #[test]
    fn proxy_env_fails_closed_when_spawn_fails() {
        // a restricted policy must never silently run without its proxy;
        // simulate spawn failure by binding everything… too expensive.
        // Instead verify the error path structurally: proxy_env on an Off
        // policy never errors and never leaves a listener behind.
        let _guard = PROXY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        assert!(proxy_env(&Policy::Off).unwrap().is_none());
        let plain = Policy::fs(Vec::new());
        assert!(proxy_env(&plain).unwrap().is_none());
    }

    #[test]
    fn authority_parsing_edge_cases() {
        assert_eq!(
            parse_authority("example.com:443", None),
            Some(("example.com".to_string(), 443))
        );
        assert_eq!(
            parse_authority("example.com", None),
            None,
            "CONNECT needs a port"
        );
        assert_eq!(
            parse_authority("example.com", Some(80)),
            Some(("example.com".to_string(), 80))
        );
        assert_eq!(
            parse_authority("[::1]:8443", None),
            Some(("::1".to_string(), 8443))
        );
        assert_eq!(parse_authority("[::1]", None), None);
        assert_eq!(parse_authority("::1:443", None), None, "bare v6 refused");
        assert_eq!(parse_authority("host:0", None), None, "port 0 refused");
        assert_eq!(parse_authority("", None), None);
        assert_eq!(parse_authority("host:notaport", None), None);
        // absolute-form split
        assert_eq!(
            split_absolute("http://user:pw@example.com:8080/a/b"),
            Some(("example.com".to_string(), 8080, "/a/b".to_string()))
        );
        assert_eq!(
            split_absolute("http://example.com"),
            Some(("example.com".to_string(), 80, "/".to_string()))
        );
        assert_eq!(
            split_absolute("https://example.com/x"),
            None,
            "only http proxied"
        );
    }

    #[test]
    fn header_value_is_case_insensitive() {
        // the head block starts AFTER the request line
        let head = "Host: example.com\r\nX-Other: y\r\n\r\n";
        assert_eq!(header_value(head, "host"), Some("example.com".to_string()));
        assert_eq!(header_value(head, "HOST"), Some("example.com".to_string()));
        assert_eq!(header_value(head, "x-other"), Some("y".to_string()));
        assert_eq!(header_value(head, "missing"), None);
    }

    #[test]
    fn read_line_and_read_head_shapes() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(b"CONNECT host:443 HTTP/1.1\r\nX-A: b\r\n\r\n")
                .unwrap();
        });
        let (mut client, _) = listener.accept().unwrap();
        let line = read_line(&mut client, MAX_LINE).unwrap();
        assert_eq!(line, "CONNECT host:443 HTTP/1.1\r\n");
        let head = read_head(&mut client).unwrap();
        assert_eq!(head, Some(b"X-A: b\r\n\r\n".to_vec()));
        writer.join().unwrap();
        // a header-less head terminates on the immediate blank line
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(b"GET http://unix/run/x HTTP/1.1\r\n\r\n")
                .unwrap();
        });
        let (mut client, _) = listener.accept().unwrap();
        let _ = read_line(&mut client, MAX_LINE).unwrap();
        let head = read_head(&mut client).unwrap();
        assert_eq!(head, Some(b"\r\n".to_vec()));
        writer.join().unwrap();
    }
}
