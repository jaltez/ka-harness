//! Credential storage behind a tiny trait: the OS keyring under service
//! `ka-passport` (ratified: same class as ka's own keys) with an
//! in-memory fallback for tests and keyring-less environments.

use std::collections::HashMap;

/// One credential slot (per vendor + kind). Values are opaque strings —
/// the Claude source stores one JSON blob per vendor.
pub trait TokenStore: Send + Sync {
    /// Read a stored value (`None` = absent). Backend failures read as
    /// absent, matching the best-effort keyring ladder in ka-dialect.
    fn load(&self, key: &str) -> Option<String>;
    /// Write a stored value (created or replaced).
    fn save(&self, key: &str, value: &str) -> Result<(), String>;
    /// Remove a stored value. Deleting an absent key is success.
    fn delete(&self, key: &str) -> Result<(), String>;
}

/// The real store: OS keyring entries under
/// [`ka_dialect::auth::PASSPORT_KEYRING_SERVICE`].
pub struct KeyringStore;

impl TokenStore for KeyringStore {
    fn load(&self, key: &str) -> Option<String> {
        let entry = keyring::Entry::new(ka_dialect::auth::PASSPORT_KEYRING_SERVICE, key).ok()?;
        match entry.get_password() {
            Ok(v) if !v.is_empty() => Some(v),
            _ => None,
        }
    }

    fn save(&self, key: &str, value: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(ka_dialect::auth::PASSPORT_KEYRING_SERVICE, key)
            .map_err(|e| format!("keyring unavailable: {e}"))?;
        entry
            .set_password(value)
            .map_err(|e| format!("keyring write failed: {e}"))
    }

    fn delete(&self, key: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(ka_dialect::auth::PASSPORT_KEYRING_SERVICE, key)
            .map_err(|e| format!("keyring unavailable: {e}"))?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            // an absent entry is already the logged-out state
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("keyring delete failed: {e}")),
        }
    }
}

/// In-memory store: tests, `--safe-mode` sandboxes without a keyring
/// daemon, and the documented fallback when the OS backend is dead
/// (credentials live only for this process — never persisted).
#[derive(Default)]
pub struct MemoryStore {
    map: parking_lot::Mutex<HashMap<String, String>>,
}

impl TokenStore for MemoryStore {
    fn load(&self, key: &str) -> Option<String> {
        self.map.lock().get(key).cloned()
    }

    fn save(&self, key: &str, value: &str) -> Result<(), String> {
        self.map.lock().insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), String> {
        self.map.lock().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn memory_store_roundtrips_and_deletes() {
        let s = MemoryStore::default();
        assert_eq!(s.load("k"), None);
        s.save("k", "v1").unwrap();
        assert_eq!(s.load("k"), Some("v1".into()));
        s.save("k", "v2").unwrap();
        assert_eq!(s.load("k"), Some("v2".into()), "save replaces");
        s.delete("k").unwrap();
        assert_eq!(s.load("k"), None);
        // deleting an absent key stays success
        s.delete("k").unwrap();
    }

    #[test]
    fn keyring_service_is_the_ratified_name() {
        assert_eq!(ka_dialect::auth::PASSPORT_KEYRING_SERVICE, "ka-passport");
    }
}
