//! Shared cache for `payer_getTerms` responses.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use serde_json::Value;

/// Upper bound on a cached `payer_getTerms` response, in seconds.
pub const MAX_GET_TERMS_CACHE_SECS: u64 = 300;

/// One shared `payer_getTerms` body, refreshed from the offer `ttl`.
#[derive(Debug)]
pub struct TermsCache {
    entry: Mutex<Option<CachedTerms>>,
}

#[derive(Debug, Clone)]
struct CachedTerms {
    response: Value,
    expires_at: Instant,
}

impl TermsCache {
    /// Creates an empty cache.
    pub const fn new() -> Self {
        Self { entry: Mutex::new(None) }
    }

    /// Returns the cached response while its `ttl` still holds.
    pub fn get(&self) -> Option<Value> {
        let mut entry = self.entry.lock().unwrap_or_else(|error| error.into_inner());
        let expired = entry.as_ref().is_some_and(|cached| Instant::now() >= cached.expires_at);
        if expired {
            *entry = None;
            return None;
        }
        entry.as_ref().map(|cached| cached.response.clone())
    }

    /// Stores `response` until the shortest offer `ttl`, capped at
    /// [`MAX_GET_TERMS_CACHE_SECS`]. A response with no positive `ttl` is left uncached.
    pub fn insert(&self, response: &Value) {
        let Some(ttl_secs) = Self::ttl_secs(response) else {
            return;
        };
        let mut entry = self.entry.lock().unwrap_or_else(|error| error.into_inner());
        *entry = Some(CachedTerms {
            response: response.clone(),
            expires_at: Instant::now() + Duration::from_secs(ttl_secs),
        });
    }

    /// Shortest positive offer `ttl` in `response`, capped at [`MAX_GET_TERMS_CACHE_SECS`].
    fn ttl_secs(response: &Value) -> Option<u64> {
        let options = response.get("options")?.as_array()?;
        let ttl_secs = options.iter().filter_map(|option| option.get("ttl")?.as_u64()).min()?;
        let ttl_secs = ttl_secs.min(MAX_GET_TERMS_CACHE_SECS);
        (ttl_secs > 0).then_some(ttl_secs)
    }
}

impl Default for TermsCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn terms(ttl: u64) -> Value {
        json!({
            "options": [{
                "kind": "token",
                "ttl": ttl,
                "tokens": [{ "token": "native", "symbol": "ETH", "decimals": 18, "rate": "0x1" }]
            }]
        })
    }

    #[test]
    fn caches_a_response_for_its_ttl() {
        let cache = TermsCache::new();
        assert!(cache.get().is_none());

        cache.insert(&terms(60));

        assert_eq!(cache.get(), Some(terms(60)));
    }

    #[test]
    fn skips_responses_without_a_positive_ttl() {
        let cache = TermsCache::new();
        cache.insert(&json!({ "options": [{ "kind": "token" }] }));
        cache.insert(&terms(0));

        assert!(cache.get().is_none());
    }

    #[test]
    fn uses_the_shortest_offer_ttl() {
        let cache = TermsCache::new();
        cache.insert(&json!({
            "options": [{ "kind": "token", "ttl": 60 }, { "kind": "token", "ttl": 15 }]
        }));

        let CachedTerms { expires_at, .. } =
            cache.entry.lock().unwrap().clone().expect("cached response");
        let remaining = expires_at.saturating_duration_since(Instant::now());
        assert!(remaining <= Duration::from_secs(15));
        assert!(remaining > Duration::from_secs(10));
    }
}
