//! Shared cache for `payer_getTerms` responses.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};

/// Upper bound on a cached `payer_getTerms` response, in seconds.
pub const MAX_GET_TERMS_CACHE_SECS: u64 = 300;

/// Payer rejection codes that mean cached terms may be stale.
const STALE_TERMS_CODES: [&str; 2] = ["PAYMENT_INSUFFICIENT", "TEMPORARILY_UNAVAILABLE"];

/// One shared `payer_getTerms` body, refetched once per block.
///
/// An entry is fresh for the block it was fetched at. Past that block it is
/// served only while its offer `ttl` holds and the payer cannot be reached,
/// with every offer `ttl` reduced by the entry's age so a wallet never holds
/// a rate longer than the payer honors it.
#[derive(Debug)]
pub struct TermsCache {
    entry: Mutex<Option<CachedTerms>>,
    refresh: AsyncMutex<()>,
}

#[derive(Debug, Clone)]
struct CachedTerms {
    response: Value,
    block: u64,
    fetched_at: Instant,
    ttl: Duration,
    /// Set by [`TermsCache::invalidate`]; refetched even within `block`.
    stale: bool,
    /// Fetched within `block` after an invalidation, so further
    /// invalidations in `block` are ignored.
    forced: bool,
}

impl CachedTerms {
    /// The response with every offer `ttl` reduced by the entry's age, or
    /// `None` once the shortest has run out.
    fn served(&self, now: Instant) -> Option<Value> {
        let age = now.saturating_duration_since(self.fetched_at);
        if age >= self.ttl {
            return None;
        }
        let age_secs = age.as_secs();
        let mut response = self.response.clone();
        for option in Self::options(&mut response) {
            if let Some(ttl) = option.get("ttl").and_then(Value::as_u64) {
                option["ttl"] = ttl.saturating_sub(age_secs).into();
            }
        }
        Some(response)
    }

    fn options(response: &mut Value) -> impl Iterator<Item = &mut Value> {
        response.get_mut("options").and_then(Value::as_array_mut).into_iter().flatten()
    }
}

impl TermsCache {
    /// Creates an empty cache.
    pub fn new() -> Self {
        Self { entry: Mutex::new(None), refresh: AsyncMutex::new(()) }
    }

    /// Returns the response fetched at `block`, unless it was invalidated or
    /// its `ttl` ran out.
    pub fn get(&self, block: u64) -> Option<Value> {
        self.get_at(block, Instant::now())
    }

    /// Returns the cached response from any block while its `ttl` holds, for
    /// when the payer cannot be reached.
    pub fn get_stale(&self) -> Option<Value> {
        self.lock().as_ref()?.served(Instant::now())
    }

    /// Stores `response` as fetched at `block`, until the shortest offer
    /// `ttl`, capped at [`MAX_GET_TERMS_CACHE_SECS`]. A response with no
    /// positive `ttl`, such as one with no offers, clears the cache.
    pub fn insert(&self, block: u64, response: &Value) {
        self.insert_at(block, response, Instant::now());
    }

    /// Drops the cached response, for when the payer answers with an error.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    /// Marks the response stale after `rejection`, a `payer_sendTransaction`
    /// error's `data`, says the payer requoted or paused, so the next request
    /// refetches. Ignored for a response already refetched for that reason
    /// at `block`, so rejections refetch at most once per block.
    pub fn invalidate(&self, block: u64, rejection: Option<&Value>) {
        let code = rejection.and_then(|data| data.get("code")).and_then(Value::as_str);
        if !code.is_some_and(|code| STALE_TERMS_CODES.contains(&code)) {
            return;
        }
        if let Some(entry) =
            self.lock().as_mut().filter(|entry| !(entry.forced && entry.block == block))
        {
            entry.stale = true;
        }
    }

    /// Waits for any other refetch to finish, so concurrent misses share one
    /// upstream call.
    pub async fn refreshing(&self) -> MutexGuard<'_, ()> {
        self.refresh.lock().await
    }

    /// Lists each token offer's `tokens` with the request's
    /// `preferredTokens` first, in their order, as the payer would for that
    /// request.
    pub fn order_for(request: &Value, mut response: Value) -> Value {
        let preferred: Vec<String> = request
            .get("preferredTokens")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|asset| asset.as_str().map(str::to_ascii_lowercase))
            .collect();
        if preferred.is_empty() {
            return response;
        }
        let rank = |token: &Value| {
            let token = token.get("token").and_then(Value::as_str).map(str::to_ascii_lowercase);
            token
                .and_then(|token| preferred.iter().position(|preferred| *preferred == token))
                .unwrap_or(usize::MAX)
        };
        for option in CachedTerms::options(&mut response) {
            if let Some(tokens) = option.get_mut("tokens").and_then(Value::as_array_mut) {
                tokens.sort_by_key(rank);
            }
        }
        response
    }

    fn get_at(&self, block: u64, now: Instant) -> Option<Value> {
        let entry = self.lock();
        let entry = entry.as_ref().filter(|entry| entry.block == block && !entry.stale)?;
        entry.served(now)
    }

    fn insert_at(&self, block: u64, response: &Value, now: Instant) {
        let mut entry = self.lock();
        let Some(ttl_secs) = Self::ttl_secs(response) else {
            *entry = None;
            return;
        };
        let forced = entry.as_ref().is_some_and(|entry| entry.stale && entry.block == block);
        *entry = Some(CachedTerms {
            response: response.clone(),
            block,
            fetched_at: now,
            ttl: Duration::from_secs(ttl_secs),
            stale: false,
            forced,
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<CachedTerms>> {
        self.entry.lock().unwrap_or_else(|error| error.into_inner())
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

    const BLOCK: u64 = 7;

    fn terms(ttl: u64) -> Value {
        json!({
            "options": [{
                "kind": "token",
                "ttl": ttl,
                "tokens": [
                    { "token": "native", "symbol": "ETH" },
                    { "token": "0x00000000000000000000000000000000000000AA", "symbol": "USDC" }
                ]
            }]
        })
    }

    fn rejection(code: &str) -> Value {
        json!({ "code": code, "message": "requote" })
    }

    #[test]
    fn serves_a_response_only_for_its_block() {
        let cache = TermsCache::new();
        assert!(cache.get(BLOCK).is_none());

        cache.insert(BLOCK, &terms(60));

        assert_eq!(cache.get(BLOCK), Some(terms(60)));
        assert!(cache.get(BLOCK + 1).is_none());
    }

    #[test]
    fn reduces_each_ttl_by_the_entry_age() {
        let cache = TermsCache::new();
        let fetched_at = Instant::now();
        cache.insert_at(BLOCK, &terms(60), fetched_at);

        let served = cache.get_at(BLOCK, fetched_at + Duration::from_millis(45_500)).unwrap();

        assert_eq!(served["options"][0]["ttl"], 15);
        assert!(cache.get_at(BLOCK, fetched_at + Duration::from_secs(60)).is_none());
    }

    #[test]
    fn serves_a_stale_block_only_while_its_ttl_holds() {
        let cache = TermsCache::new();
        cache.insert(BLOCK, &terms(60));
        cache.invalidate(BLOCK, Some(&rejection("PAYMENT_INSUFFICIENT")));

        assert!(cache.get(BLOCK).is_none());
        assert_eq!(cache.get_stale(), Some(terms(60)));

        let expired = TermsCache::new();
        expired.insert_at(BLOCK, &terms(1), Instant::now() - Duration::from_secs(1));
        assert!(expired.get_stale().is_none());
    }

    #[test]
    fn clears_on_a_response_without_offers() {
        let cache = TermsCache::new();
        cache.insert(BLOCK, &terms(60));

        cache.insert(BLOCK + 1, &json!({ "options": [] }));

        assert!(cache.get_stale().is_none());
    }

    #[test]
    fn skips_responses_without_a_positive_ttl() {
        let cache = TermsCache::new();
        cache.insert(BLOCK, &json!({ "options": [{ "kind": "token" }] }));
        cache.insert(BLOCK, &terms(0));

        assert!(cache.get(BLOCK).is_none());
    }

    #[test]
    fn uses_the_shortest_offer_ttl_capped() {
        let cache = TermsCache::new();
        let fetched_at = Instant::now();
        let response = json!({
            "options": [{ "kind": "token", "ttl": 60 }, { "kind": "token", "ttl": 15 }]
        });
        cache.insert_at(BLOCK, &response, fetched_at);
        assert!(cache.get_at(BLOCK, fetched_at + Duration::from_secs(15)).is_none());

        cache.insert_at(BLOCK, &terms(3_600), fetched_at);
        let capped = fetched_at + Duration::from_secs(MAX_GET_TERMS_CACHE_SECS);
        assert!(cache.get_at(BLOCK, capped).is_none());
    }

    #[test]
    fn refetches_at_most_once_per_block_after_rejections() {
        let cache = TermsCache::new();
        cache.insert(BLOCK, &terms(60));

        cache.invalidate(BLOCK, Some(&rejection("SENDER_BALANCE_INSUFFICIENT")));
        cache.invalidate(BLOCK, None);
        assert!(cache.get(BLOCK).is_some());

        cache.invalidate(BLOCK, Some(&rejection("PAYMENT_INSUFFICIENT")));
        assert!(cache.get(BLOCK).is_none());
        cache.insert(BLOCK, &terms(60));
        cache.invalidate(BLOCK, Some(&rejection("TEMPORARILY_UNAVAILABLE")));
        assert!(cache.get(BLOCK).is_some());

        cache.insert(BLOCK + 1, &terms(60));
        cache.invalidate(BLOCK + 1, Some(&rejection("PAYMENT_INSUFFICIENT")));
        assert!(cache.get(BLOCK + 1).is_none());
    }

    #[test]
    fn orders_tokens_by_each_request_preference() {
        let request = json!({ "preferredTokens": ["0x00000000000000000000000000000000000000aa"] });

        let ordered = TermsCache::order_for(&request, terms(60));

        let symbols: Vec<_> = ordered["options"][0]["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|token| token["symbol"].as_str().unwrap())
            .collect();
        assert_eq!(symbols, ["USDC", "ETH"]);
        assert_eq!(TermsCache::order_for(&json!({}), terms(60)), terms(60));
    }
}
