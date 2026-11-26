//! PKCE (RFC 7636) S256: code verifier + derived challenge + state, for
//! the Claude public-client OAuth flow (no client secret).
//!
//! Entropy: the OS CSPRNG via `getrandom` on every target (Linux
//! `getrandom(2)`, Windows `BCryptGenRandom`, macOS `getentropy`). Fails
//! closed with an error if the OS CSPRNG is unavailable — never falls
//! back to predictable material. The verifier is base64url
//! (`A-Z a-z 0-9 - _`, no padding), 43 chars for 32 bytes: inside the
//! RFC 7636 alphabet and length range.

use sha2::{Digest, Sha256};

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
    /// Fresh verifier/challenge/state triple from the OS CSPRNG.
    /// Fails closed if OS randomness is unavailable.
    pub fn generate() -> Result<Self, String> {
        let verifier = base64url_no_pad(&random_bytes(32)?);
        let challenge = base64url_no_pad(Sha256::digest(verifier.as_bytes()).as_slice());
        let state = base64url_no_pad(&random_bytes(16)?);
        Ok(Self {
            verifier,
            challenge,
            state,
        })
    }
}

/// The verifier must hash to the challenge (S256 binding).
pub fn challenge_matches(verifier: &str, challenge: &str) -> bool {
    base64url_no_pad(Sha256::digest(verifier.as_bytes()).as_slice()) == challenge
}

/// Fill a buffer with random bytes from the OS CSPRNG (`getrandom`).
/// Fails closed if the OS randomness source fails.
fn random_bytes(len: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf).map_err(|e| format!("OS CSPRNG unavailable: {e}"))?;
    Ok(buf)
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
        let Ok(pkce) = Pkce::generate() else {
            panic!("Pkce::generate failed");
        };
        assert!(challenge_matches(&pkce.verifier, &pkce.challenge));
        assert!(
            !challenge_matches("not-the-verifier", &pkce.challenge),
            "a wrong verifier must not match"
        );
    }

    #[test]
    fn verifier_and_state_use_rfc7636_shapes() {
        let Ok(pkce) = Pkce::generate() else {
            panic!("Pkce::generate failed");
        };
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
        let Ok(a) = Pkce::generate() else {
            panic!("Pkce::generate failed");
        };
        let Ok(b) = Pkce::generate() else {
            panic!("Pkce::generate failed");
        };
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.state, b.state);
    }

    #[test]
    fn os_csprng_yields_distinct_material_across_calls() {
        let mut verifiers = Vec::new();
        for _ in 0..64 {
            let Ok(pkce) = Pkce::generate() else {
                panic!("Pkce::generate failed");
            };
            assert_eq!(pkce.verifier.len(), 43, "length stays RFC 7636");
            assert!(!verifiers.contains(&pkce.verifier), "no verifier reuse");
            verifiers.push(pkce.verifier);
        }
        let Ok(raw) = random_bytes(32) else {
            panic!("random_bytes failed");
        };
        let Ok(again) = random_bytes(32) else {
            panic!("random_bytes failed");
        };
        assert_ne!(raw, again, "raw random_bytes never repeats");
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
