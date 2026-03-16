//! Placeholder local key-value store for node state.
//!
//! All mutable node state (identity, invites, contacts, persona data,
//! imported namespaces) lives here as the single source of truth.
//!
//! TODO(persistent-storage): Replace the in-memory `HashMap` with redb
//! or Willow `_sys/` namespace paths for persistence and cross-device
//! replication.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Error type for [`LocalStore`] operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum StoreError {
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("lock poisoned")]
    LockPoisoned,
}

/// A thread-safe, cloneable in-memory KV store.
///
/// Keys are hierarchical strings separated by `/`.
/// Values are opaque byte vectors (typically JSON).
///
/// Clone is cheap (`Arc`).
#[derive(Clone)]
pub struct LocalStore {
    inner: Arc<RwLock<HashMap<String, Vec<u8>>>>,
}

impl Default for LocalStore {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    // -- Raw byte operations --

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let guard = self.inner.read().map_err(|_| StoreError::LockPoisoned)?;
        Ok(guard.get(key).cloned())
    }

    pub fn set(&self, key: &str, value: Vec<u8>) -> Result<(), StoreError> {
        let mut guard = self.inner.write().map_err(|_| StoreError::LockPoisoned)?;
        guard.insert(key.to_owned(), value);
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<bool, StoreError> {
        let mut guard = self.inner.write().map_err(|_| StoreError::LockPoisoned)?;
        Ok(guard.remove(key).is_some())
    }

    /// Return all keys matching the given prefix.
    pub fn keys(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let guard = self.inner.read().map_err(|_| StoreError::LockPoisoned)?;
        Ok(guard
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }

    /// Delete all keys matching the given prefix. Returns count deleted.
    pub fn delete_prefix(&self, prefix: &str) -> Result<usize, StoreError> {
        let mut guard = self.inner.write().map_err(|_| StoreError::LockPoisoned)?;
        let to_remove: Vec<String> = guard
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        let count = to_remove.len();
        for k in to_remove {
            guard.remove(&k);
        }
        Ok(count)
    }

    /// Check if a key exists.
    pub fn contains(&self, key: &str) -> Result<bool, StoreError> {
        let guard = self.inner.read().map_err(|_| StoreError::LockPoisoned)?;
        Ok(guard.contains_key(key))
    }

    // -- Typed JSON operations --

    pub fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StoreError> {
        match self.get(key)? {
            Some(bytes) => {
                let val =
                    serde_json::from_slice(&bytes).map_err(|e| StoreError::Serde(e.to_string()))?;
                Ok(Some(val))
            }
            None => Ok(None),
        }
    }

    pub fn set_json<T: Serialize>(&self, key: &str, value: &T) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(value).map_err(|e| StoreError::Serde(e.to_string()))?;
        self.set(key, bytes)
    }

    /// Read all values matching a key prefix, deserializing each as JSON.
    pub fn values_json<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<T>, StoreError> {
        let guard = self.inner.read().map_err(|_| StoreError::LockPoisoned)?;
        let mut out = Vec::new();
        for (k, v) in &*guard {
            if k.starts_with(prefix) {
                let val =
                    serde_json::from_slice(v).map_err(|e| StoreError::Serde(e.to_string()))?;
                out.push(val);
            }
        }
        Ok(out)
    }
}

/// Key prefix conventions for the local store.
///
/// All key-building functions accept `&impl Display` so they work
/// with any byte-ID newtype (`Aid`, `NamespaceId`, etc.).
pub mod keys {
    use std::fmt::Display;

    pub const CURRENT_AID: &str = "identity/current_aid";

    pub fn invite(aid: &impl Display) -> String {
        format!("invites/{aid}")
    }
    pub const INVITES_PREFIX: &str = "invites/";

    pub fn contact(aid: &impl Display) -> String {
        format!("contacts/{aid}")
    }
    pub const CONTACTS_PREFIX: &str = "contacts/";

    pub fn persona(key: &str) -> String {
        format!("persona/{key}")
    }
    pub const PERSONA_PREFIX: &str = "persona/";

    pub fn pending_invite(subspace: &impl Display, namespace: &impl Display) -> String {
        format!("pending_invites/{subspace}/{namespace}")
    }
    pub const PENDING_INVITES_PREFIX: &str = "pending_invites/";

    pub fn namespace_imported(ns: &impl Display) -> String {
        format!("namespaces/imported/{ns}")
    }
    pub const NAMESPACES_IMPORTED_PREFIX: &str = "namespaces/imported/";
}
