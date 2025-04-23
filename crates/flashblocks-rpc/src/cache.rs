use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct CacheEntry<T> {
    value: T,
    expiry: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct Cache {
    store: Arc<RwLock<HashMap<String, CacheEntry<Vec<u8>>>>>,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            store: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Cache {
    pub fn set<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl_secs: Option<u64>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let serialized = serde_json::to_vec(value)?;
        let entry = CacheEntry {
            value: serialized,
            expiry: ttl_secs.map(|secs| Instant::now() + Duration::from_secs(secs)),
        };

        let mut store = self.store.write().unwrap();
        store.insert(key.to_string(), entry);
        Ok(())
    }

    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let store = self.store.read().unwrap();
        store.get(key).and_then(|entry| {
            if entry.expiry.is_some_and(|e| Instant::now() > e) {
                return None;
            }
            serde_json::from_slice(&entry.value).ok()
        })
    }

    pub fn cleanup_expired(&self) {
        if let Ok(mut store) = self.store.write() {
            store.retain(|_, entry| {
                entry
                    .expiry
                    .map(|expiry| Instant::now() <= expiry)
                    .unwrap_or(true)
            });
        }
    }
}
