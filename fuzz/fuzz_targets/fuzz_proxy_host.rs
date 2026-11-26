#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut lines = text.lines();
    let allow_raw = lines.next().unwrap_or("");
    let deny_raw = lines.next().unwrap_or("");
    let allow: Vec<String> = allow_raw
        .split(',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let deny: Vec<String> = deny_raw
        .split(',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    let filter = ka_sandbox::proxy::DomainFilter::new(&allow, &deny);
    let _ = filter.is_empty();
    for host in lines {
        let _ = filter.permits(host);
        let _ = filter.allow_matches(host);
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            let _ = ka_sandbox::proxy::ip_is_local(ip);
        }
    }
});
