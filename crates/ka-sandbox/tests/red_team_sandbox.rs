//! Red-team regression suite for `ka-sandbox`:
//! - HTTP proxy first-header preservation + `Connection: close` enforcement
//! - Trailing-dot DNS bypass & `normalize_entry`
//! - Anti-DNS-rebinding (allowlisted DNS domains never exempt from `ip_is_local`)
//! - Expanded `ip_is_local` CIDR/IPv6 transition matrix
//! - HTTP smuggling & authority parser hardening
//! - macOS Seatbelt SBPL last-match-wins rule order & `/usr/bin/sandbox-exec`
//! - Windows fail-closed detection & Landlock-over-Firejail priority

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use ka_sandbox::proxy::{DomainFilter, NetProxy, check_resolved_ip, ip_is_local, resolve_pinned};
use ka_sandbox::{
    NetDomains, Policy, Tool, detect_tool_for_os, seatbelt_argv, seatbelt_profile,
    seatbelt_profile_check, seatbelt_profile_with_port,
};

fn filter(allow: &[&str], deny: &[&str]) -> DomainFilter {
    let allow: Vec<String> = allow.iter().map(|s| (*s).to_string()).collect();
    let deny: Vec<String> = deny.iter().map(|s| (*s).to_string()).collect();
    DomainFilter::new(&allow, &deny)
}

fn roundtrip(proxy_addr: SocketAddr, request: &str) -> String {
    let Ok(mut client) = TcpStream::connect(proxy_addr) else {
        panic!("failed to connect to proxy at {proxy_addr}");
    };
    if let Err(e) = client.set_read_timeout(Some(Duration::from_secs(5))) {
        panic!("set_read_timeout failed: {e}");
    }
    if let Err(e) = client.write_all(request.as_bytes()) {
        panic!("write_all failed: {e}");
    }
    let mut buf = Vec::new();
    let _ = client.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Spawn a local TCP capture server that records the raw HTTP head it
/// received and answers `200 OK` with body `captured-ok`.
fn spawn_capture_server() -> (u16, mpsc::Receiver<String>) {
    let Ok(listener) = TcpListener::bind(("127.0.0.1", 0)) else {
        panic!("failed to bind capture listener");
    };
    let Ok(local_addr) = listener.local_addr() else {
        panic!("failed to read capture local_addr");
    };
    let port = local_addr.port();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut received = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if received.len() >= 64 * 1024 {
                break;
            }
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    received.push(byte[0]);
                    if received.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&received).into_owned());
        let body = "captured-ok";
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.flush();
    });
    (port, rx)
}

#[test]
fn serve_http_preserves_first_header_and_forces_connection_close() {
    let (port, rx) = spawn_capture_server();
    let Ok(proxy) = NetProxy::spawn(filter(&["127.0.0.1"], &[])) else {
        panic!("failed to spawn proxy");
    };
    let req = format!(
        "GET http://127.0.0.1:{port}/items?x=1 HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         X-Custom: preserve-me\r\n\
         Connection: keep-alive\r\n\
         Proxy-Connection: keep-alive\r\n\
         \r\n"
    );
    let resp = roundtrip(proxy.addr(), &req);
    assert!(resp.contains("200 OK"), "unexpected response: {resp}");
    assert!(resp.contains("captured-ok"), "unexpected body: {resp}");

    let Ok(captured) = rx.recv_timeout(Duration::from_secs(5)) else {
        panic!("capture server did not receive forwarded request");
    };
    assert!(
        captured.starts_with("GET /items?x=1 HTTP/1.1\r\n"),
        "request line not rewritten cleanly: {captured:?}"
    );
    assert!(
        captured.contains("Host: 127.0.0.1\r\n"),
        "first header (Host) was dropped: {captured:?}"
    );
    assert!(
        captured.contains("X-Custom: preserve-me\r\n"),
        "custom header was dropped: {captured:?}"
    );
    assert!(
        captured.contains("Connection: close\r\n"),
        "Connection: close was not injected: {captured:?}"
    );
    assert!(
        !captured.to_ascii_lowercase().contains("keep-alive"),
        "keep-alive must be stripped: {captured:?}"
    );
    assert!(
        !captured.to_ascii_lowercase().contains("proxy-connection"),
        "Proxy-Connection must be stripped: {captured:?}"
    );
}

#[test]
fn serve_http_handles_headerless_request_cleanly() {
    let (port, rx) = spawn_capture_server();
    let Ok(proxy) = NetProxy::spawn(filter(&["127.0.0.1"], &[])) else {
        panic!("failed to spawn proxy");
    };
    let req = format!("GET http://127.0.0.1:{port}/bare HTTP/1.1\r\n\r\n");
    let resp = roundtrip(proxy.addr(), &req);
    assert!(resp.contains("captured-ok"), "unexpected response: {resp}");

    let Ok(captured) = rx.recv_timeout(Duration::from_secs(5)) else {
        panic!("capture server did not receive bare request");
    };
    assert_eq!(captured, "GET /bare HTTP/1.1\r\nConnection: close\r\n\r\n");
}

#[test]
fn trailing_dot_dns_bypass_and_entry_normalization() {
    let denied = filter(&[], &["evil.com"]);
    assert!(!denied.permits("evil.com"), "bare denied domain");
    assert!(
        !denied.permits("evil.com."),
        "trailing dot must not bypass deny rule"
    );
    assert!(
        !denied.permits("sub.evil.com."),
        "subdomain with trailing dot must not bypass deny rule"
    );
    assert!(
        !denied.permits("EVIL.COM.."),
        "multiple trailing dots must not bypass deny rule"
    );

    // normalize_entry("example.com.") must strip the trailing dot without
    // reverting to the unstripped string
    let allowed = filter(&["example.com."], &[]);
    assert!(
        allowed.permits("example.com"),
        "trailing dot in config entry must normalize to apex"
    );
    assert!(
        allowed.permits("sub.example.com"),
        "trailing dot in config entry must cover subdomains"
    );
    assert!(allowed.permits("example.com."));
    assert!(!allowed.permits("other.com"));

    let wildcard = filter(&["*.wild.example."], &[]);
    assert!(wildcard.permits("wild.example"));
    assert!(wildcard.permits("a.wild.example."));
    assert!(!wildcard.permits("notwild.example"));
}

#[test]
fn anti_dns_rebinding_rejects_allowlisted_dns_domain_resolving_to_local_ip() {
    // Even when a DNS domain is explicitly in net_allow_domains, if it
    // resolves to a loopback/private/link-local IP it MUST be refused.
    let f = filter(&["rebind.example.com", "*.corp.example"], &[]);
    assert!(f.permits("rebind.example.com"));
    assert!(
        check_resolved_ip(&f, "rebind.example.com", IpAddr::V4(Ipv4Addr::LOCALHOST)).is_err(),
        "allowlisted DNS domain resolving to 127.0.0.1 must be denied"
    );
    assert!(
        check_resolved_ip(
            &f,
            "rebind.example.com",
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        )
        .is_err(),
        "allowlisted DNS domain resolving to metadata IP must be denied"
    );
    assert!(
        check_resolved_ip(
            &f,
            "api.corp.example",
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))
        )
        .is_err(),
        "allowlisted wildcard DNS domain resolving to RFC1918 must be denied"
    );
    assert!(
        check_resolved_ip(
            &f,
            "rebind.example.com",
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))
        )
        .is_ok(),
        "public IP for allowlisted DNS domain remains permitted"
    );

    // Literal 127.0.0.1 is permitted ONLY when explicitly allowlisted
    let f_ip = filter(&["127.0.0.1"], &[]);
    assert!(
        resolve_pinned(&f_ip, "127.0.0.1", 8080).is_ok(),
        "explicitly allowlisted IP literal 127.0.0.1 must be permitted"
    );
    let f_empty = filter(&[], &[]);
    assert!(
        resolve_pinned(&f_empty, "127.0.0.1", 8080).is_err(),
        "non-allowlisted IP literal 127.0.0.1 must be denied"
    );
    // `localhost` is permitted when explicitly allowlisted
    let f_lh = filter(&["localhost"], &[]);
    assert!(
        check_resolved_ip(&f_lh, "localhost", IpAddr::V4(Ipv4Addr::LOCALHOST)).is_ok(),
        "explicitly allowlisted localhost must be permitted"
    );
}

#[test]
fn ip_is_local_expanded_matrix() {
    let must_be_local = [
        "100.64.0.1",
        "100.100.100.200",
        "0.1.2.3",
        "198.18.0.1",
        "192.0.0.1",
        "224.0.0.1",
        "240.0.0.1",
        "64:ff9b::169.254.169.254",
        "::ffff:100.64.0.1",
        "::127.0.0.1",
        "2002::1",
        "2001::1",
        "ff02::1",
    ];
    for raw in must_be_local {
        let Ok(ip) = raw.parse::<IpAddr>() else {
            panic!("test IP {raw} failed to parse");
        };
        assert!(ip_is_local(ip), "{raw} must be classified as local");
    }

    let must_be_public = ["8.8.8.8", "1.1.1.1", "2606:4700::1111", "::ffff:8.8.8.8"];
    for raw in must_be_public {
        let Ok(ip) = raw.parse::<IpAddr>() else {
            panic!("test IP {raw} failed to parse");
        };
        assert!(!ip_is_local(ip), "{raw} must remain public");
    }
}

#[test]
fn http_smuggling_and_authority_hardening() {
    let (port, _rx) = spawn_capture_server();
    let Ok(proxy) = NetProxy::spawn(filter(&["127.0.0.1"], &["evil.example"])) else {
        panic!("failed to spawn proxy");
    };

    // 1. Duplicate Host headers -> 400 Bad Request
    let dup_host = format!(
        "GET http://127.0.0.1:{port}/ HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Host: 127.0.0.1\r\n\r\n"
    );
    let resp = roundtrip(proxy.addr(), &dup_host);
    assert!(
        resp.contains("400 Bad Request"),
        "duplicate Host must be rejected with 400: {resp}"
    );

    // 2. Host / absolute-URI mismatch -> 400 Bad Request
    let mismatch = format!(
        "GET http://127.0.0.1:{port}/ HTTP/1.1\r\n\
         Host: evil.example\r\n\r\n"
    );
    let resp = roundtrip(proxy.addr(), &mismatch);
    assert!(
        resp.contains("400 Bad Request"),
        "Host/URI mismatch must be rejected with 400: {resp}"
    );

    // 3. Uppercase HTTP:// must be parsed as absolute-form (not origin-form)
    let upper_denied = "GET HTTP://evil.example/secret HTTP/1.1\r\nHost: evil.example\r\n\r\n";
    let resp = roundtrip(proxy.addr(), upper_denied);
    assert!(
        resp.contains("403 Forbidden"),
        "uppercase HTTP:// to denied host must be 403: {resp}"
    );

    let upper_ok = format!(
        "GET HTTP://127.0.0.1:{port}/upper HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\r\n"
    );
    let resp = roundtrip(proxy.addr(), &upper_ok);
    assert!(
        resp.contains("captured-ok"),
        "uppercase HTTP:// to allowlisted host must succeed: {resp}"
    );

    // 4. Bracketed IPv6 port 0 -> 400 Bad Request
    let resp = roundtrip(proxy.addr(), "CONNECT [::1]:0 HTTP/1.1\r\n\r\n");
    assert!(
        resp.contains("400 Bad Request"),
        "[::1]:0 must be rejected with 400: {resp}"
    );

    // 5. Bracketed IPv6 trailing garbage -> 400 Bad Request
    let resp = roundtrip(proxy.addr(), "CONNECT [::1]junk HTTP/1.1\r\n\r\n");
    assert!(
        resp.contains("400 Bad Request"),
        "[::1]junk must be rejected with 400: {resp}"
    );
}

#[test]
fn seatbelt_profile_rule_order_and_hardening() {
    let restricted = Policy::Fs {
        allow_write: vec![PathBuf::from("/tmp/work")],
        allow_net: false,
        pass_env: Vec::new(),
        net: NetDomains {
            allow: vec!["api.example.com".to_string()],
            deny: Vec::new(),
        },
    };

    let profile = seatbelt_profile(&restricted);
    let Some(deny_pos) = profile.find("(deny network*)") else {
        panic!("missing (deny network*) in profile: {profile}");
    };
    let Some(allow_pos) = profile.find("(allow network-outbound (remote ip \"127.0.0.1\"))") else {
        panic!("missing loopback allow in profile: {profile}");
    };
    assert!(
        deny_pos < allow_pos,
        "SBPL is last-match-wins: (deny network*) must appear BEFORE (allow network-outbound ...): {profile}"
    );
    assert!(
        !profile.contains("/dev/shm"),
        "/dev/shm must not be writable by default: {profile}"
    );

    let scoped = seatbelt_profile_with_port(&restricted, Some(45678));
    let Some(deny_pos) = scoped.find("(deny network*)") else {
        panic!("missing (deny network*) in port-scoped profile: {scoped}");
    };
    let Some(allow_pos) = scoped.find("(allow network-outbound (remote ip \"127.0.0.1:45678\"))")
    else {
        panic!("missing port-scoped loopback allow in profile: {scoped}");
    };
    assert!(
        deny_pos < allow_pos,
        "port-scoped loopback allow must follow (deny network*): {scoped}"
    );

    let argv = seatbelt_argv(&profile, "true");
    assert_eq!(
        argv.first().map(String::as_str),
        Some("/usr/bin/sandbox-exec"),
        "seatbelt_argv must use absolute /usr/bin/sandbox-exec: {argv:?}"
    );

    // Defense-in-depth newline/CR escaping in seatbelt_quote
    let newline_policy = Policy::fs(vec![PathBuf::from("/tmp/a\n(allow file-write*)\r")]);
    assert!(seatbelt_profile_check(&newline_policy).is_err());
    let escaped_profile = seatbelt_profile(&newline_policy);
    assert!(
        escaped_profile.contains("\\n(allow file-write*)\\r"),
        "newlines and CRs must be escaped in SBPL string literals: {escaped_profile}"
    );
}

#[test]
fn windows_fail_closed_and_landlock_priority_over_firejail() {
    // On Windows, detection MUST always return None even if bwrap/firejail
    // or any other backend reports available.
    assert_eq!(
        detect_tool_for_os(true, true, true, true, true),
        None,
        "Windows must always fail closed"
    );
    assert_eq!(detect_tool_for_os(true, true, false, false, false), None);

    // On non-Windows: bwrap > landlock > firejail > seatbelt
    assert_eq!(
        detect_tool_for_os(false, true, true, true, true),
        Some(Tool::Bubblewrap)
    );
    assert_eq!(
        detect_tool_for_os(false, false, true, true, true),
        Some(Tool::Landlock),
        "Landlock must take priority over Firejail"
    );
    assert_eq!(
        detect_tool_for_os(false, false, false, true, true),
        Some(Tool::Firejail)
    );
    assert_eq!(
        detect_tool_for_os(false, false, false, false, true),
        Some(Tool::Seatbelt)
    );
    assert_eq!(detect_tool_for_os(false, false, false, false, false), None);
}
