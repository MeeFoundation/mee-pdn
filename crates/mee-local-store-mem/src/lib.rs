use mee_local_store_api::{Key, KvStore, Namespace, Value};
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, RwLock};

#[derive(Clone, Default)]
pub struct MemKvStore {
    inner: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
}

impl MemKvStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[allow(clippy::expect_used, clippy::unwrap_in_result)]
impl KvStore for MemKvStore {
    fn set(&self, ns: &Namespace, key: &Key, value: &Value) -> io::Result<()> {
        let mut guard = self.inner.write().expect("MemKvStore lock poisoned");
        let map = guard.entry(ns.0.clone()).or_default();
        map.insert(key.0.clone(), value.0.clone());
        Ok(())
    }

    fn get(&self, ns: &Namespace, key: &Key) -> io::Result<Option<Value>> {
        let guard = self.inner.read().expect("MemKvStore lock poisoned");
        Ok(guard
            .get(&ns.0)
            .and_then(|m| m.get(&key.0).cloned())
            .map(Value))
    }
}
