//! PKCE (RFC 7636) S256: code verifier + derived challenge + state, for
//! the Claude public-client OAuth flow (no client secret).
//!
//! Entropy: `/dev/urandom` where present; the fallback mixes wall-clock
//! nanoseconds, the pid, a process-lifetime counter and an address
//! through SHA-256 — unique-per-login material for a verifier, not a
//! CSPRNG contract. The verifier is base64url (`A-Z a-z 0-9 - _`, no
//! padding), 43 chars for 32 bytes: inside the RFC 7636 alphabet and
//! length range.

use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

/// Everything one login attempt needs to bind the browser round-trip to
/// the token exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    /// High-entropy secret sent to the token endpoint (`code_verifier`).
    pub verifier: String,
    /// SHA-256 of the verifier, sent to the authorize endpoint
    /// (`code_challenge`, method S256).
    pub challenge: String,
    /// CSRF nonce echoed back by the OAuth callback.
    pub state: String,
}

impl Pkce {
    /// Fresh verifier/challenge/state triple.
    pub fn generate() -> Self {
        let verifier = base64url_no_pad(&random_bytes(32));
        let challenge = base64url_no_pad(Sha256::digest(verifier.as_bytes()).as_slice());
        let state = base64url_no_pad(&random_bytes(16));
        Self {
            verifier,
            challenge,
            state,
        }
    }
}

/// The verifier must hash to the challenge (S256 binding).
pub fn challenge_matches(verifier: &str, challenge: &str) -> bool {
    base64url_no_pad(Sha256::digest(verifier.as_bytes()).as_slice()) == challenge
}

/// Fill `buf` with random bytes.
fn random_bytes(len: usize) -> Vec<u8> {
    // bounded read: /dev/urandom never EOFs, `fs::read` would hang
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read as _;
        let mut buf = vec![0u8; len];
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    fallback_bytes(len)
}

/// Non-urandom fallback: mix time/pid/counter/address through SHA-256
/// and stretch to `len` by re-hashing (documented best-effort, never
/// a CSPRNG).
fn fallback_bytes(len: usize) -> Vec<u8> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut seed = Vec::new();
    let mut block = format!(
        "{}|{}|{}|{:p}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        &seed as *const _
    );
    while seed.len() < len {
        let digest = Sha256::digest(block.as_bytes()).to_vec();
        seed.extend_from_slice(&digest);
        block = digest
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",");
    }
    seed.truncate(len);
    seed
}

/// Standard base64 alphabet with the URL-unsafe pair mapped: `+` → `-`,
/// `/` → `_`; padding stripped.
fn base64url_no_pad(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn verifier_binds_to_challenge_via_s256() {
        let pkce = Pkce::generate();
        assert!(challenge_matches(&pkce.verifier, &pkce.challenge));
        assert!(
            !challenge_matches("not-the-verifier", &pkce.challenge),
            "a wrong verifier must not match"
        );
    }

    #[test]
    fn verifier_and_state_use_rfc7636_shapes() {
        let pkce = Pkce::generate();
        // 32 bytes → exactly 43 unpadded base64 chars
        assert_eq!(pkce.verifier.len(), 43);
        assert!(pkce.state.len() >= 22, "state is 16 bytes encoded");
        for s in [&pkce.verifier, &pkce.challenge, &pkce.state] {
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "base64url alphabet only: {s}"
            );
            assert!(!s.contains('='), "no padding: {s}");
        }
        assert_eq!(pkce.challenge.len(), 43);
    }

    #[test]
    fn generation_is_independent() {
        let a = Pkce::generate();
        let b = Pkce::generate();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.state, b.state);
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors, `+/` → `-_`, padding stripped
        assert_eq!(base64url_no_pad(b""), "");
        assert_eq!(base64url_no_pad(b"f"), "Zg");
        assert_eq!(base64url_no_pad(b"fo"), "Zm8");
        assert_eq!(base64url_no_pad(b"foo"), "Zm9v");
        assert_eq!(base64url_no_pad(b"foob"), "Zm9vYg");
        assert_eq!(base64url_no_pad(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_no_pad(b"foobar"), "Zm9vYmFy");
        // alphabet edge: 0xFB 0xFF → "+///" → "-___" with padding dropped
        assert_eq!(base64url_no_pad(&[0xfb, 0xff, 0xff]), "-___");
    }
}
