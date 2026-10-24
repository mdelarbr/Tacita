//! Where Tacita keeps credentials: the macOS Keychain, or memory in tests.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

pub trait SecretStore: Send + Sync {
    fn get(&self, name: &str) -> anyhow::Result<Option<String>>;
    fn set(&self, name: &str, value: &str) -> anyhow::Result<()>;
}

/// In-memory store for tests. No `Debug`, so its values can't be printed.
#[derive(Default)]
pub struct MemoryStore {
    values: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    fn values(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.values().get(name).cloned())
    }

    fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        self.values().insert(name.to_string(), value.to_string());
        Ok(())
    }
}

/// Generic passwords in the login Keychain: service `service`, account `name`.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct KeychainStore {
    service: String,
}

#[cfg(target_os = "macos")]
impl KeychainStore {
    pub fn new(service: &str) -> KeychainStore {
        KeychainStore {
            service: service.to_string(),
        }
    }
}

/// `errSecItemNotFound`.
#[cfg(target_os = "macos")]
const ITEM_NOT_FOUND: i32 = -25300;

/// Maps a Keychain read: "not found" is `None`. Error texts never carry the
/// value.
#[cfg(target_os = "macos")]
fn found(r: Result<Vec<u8>, security_framework::base::Error>) -> anyhow::Result<Option<String>> {
    match r {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| anyhow::anyhow!("the Keychain item is not valid UTF-8")),
        Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

#[cfg(target_os = "macos")]
impl SecretStore for KeychainStore {
    fn get(&self, name: &str) -> anyhow::Result<Option<String>> {
        found(security_framework::passwords::get_generic_password(
            &self.service,
            name,
        ))
    }

    fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        security_framework::passwords::set_generic_password(&self.service, name, value.as_bytes())
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip() {
        let s = MemoryStore::default();
        assert_eq!(s.get("openrouter").unwrap(), None);
        s.set("openrouter", "sk-or-v1-A").unwrap();
        s.set("openrouter", "sk-or-v1-B").unwrap();
        assert_eq!(s.get("openrouter").unwrap().as_deref(), Some("sk-or-v1-B"));
        assert_eq!(s.get("other").unwrap(), None);
    }

    // These never touch the real Keychain: they test the result mapping.
    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_not_found_is_none_and_other_errors_are_errors() {
        use security_framework::base::Error;
        assert_eq!(found(Err(Error::from_code(-25300))).unwrap(), None);
        assert!(found(Err(Error::from_code(-25293))).is_err());
        assert_eq!(
            found(Ok(b"sk-or-v1-X".to_vec())).unwrap().as_deref(),
            Some("sk-or-v1-X")
        );
        let e = found(Ok(vec![b's', b'k', 0xff])).unwrap_err().to_string();
        assert!(!e.contains("sk"), "{e}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_store_names_its_service() {
        let k = KeychainStore::new("dev.tacita.test");
        assert_eq!(
            format!("{k:?}"),
            r#"KeychainStore { service: "dev.tacita.test" }"#
        );
    }
}
