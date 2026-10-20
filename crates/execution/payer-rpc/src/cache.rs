//! Shared cache for `payer_getTerms` responses.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};

/// Upper bound on a cached `payer_getTerms` response, in seconds.
pub const MAX_GET_TERMS_CACHE_SECS: u64 = 300;

/// How long a fetched response is served before the next request refetches it.
///
/// Serving rates this old relies on the payer honoring any rate it quoted within the offer
/// `ttl`, with each served `ttl` reduced by the entry's age.
pub const GET_TERMS_FRESH_FOR: Duration = Duration::from_secs(5);

/// Rejection for a phase-0 credit below the payer's current rate.
const PAYMENT_INSUFFICIENT: &str = "PAYMENT_INSUFFICIENT";

/// Rejection while the payer is paused or cannot price.
const TEMPORARILY_UNAVAILABLE: &str = "TEMPORARILY_UNAVAILABLE";

/// One shared `payer_getTerms` body, refetched on demand at most every
/// [`GET_TERMS_FRESH_FOR`].
///
/// Past that it is served only while its offer `ttl` holds and the payer cannot be reached,
/// with every offer `ttl` reduced by the entry's age so a wallet never holds
/// a rate longer than the payer honors it.
#[derive(Debug)]
pub struct TermsCache {
    entry: Mutex<Option<CachedTerms>>,
    refresh: AsyncMutex<Option<Instant>>,
}

#[derive(Debug, Clone)]
struct CachedTerms {
    response: Value,
    fetched_at: Instant,
    ttl: Duration,
    /// Set by [`TermsCache::invalidate`]; refetched while still fresh.
    stale: bool,
    /// Fetched after an invalidation, so invalidations are ignored while it
    /// is fresh.
    forced: bool,
}

impl CachedTerms {
    fn fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.fetched_at) < GET_TERMS_FRESH_FOR
    }

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
        Self { entry: Mutex::new(None), refresh: AsyncMutex::new(None) }
    }

    /// Returns the response while it is fresh and was not invalidated.
    pub fn get(&self) -> Option<Value> {
        self.get_at(Instant::now())
    }

    /// Returns the cached response, fresh or not, while its `ttl` holds, for
    /// when the payer cannot be reached.
    pub fn get_stale(&self) -> Option<Value> {
        self.lock().as_ref()?.served(Instant::now())
    }

    /// Stores `response` until the shortest offer `ttl`, capped at
    /// [`MAX_GET_TERMS_CACHE_SECS`]. A response with no positive `ttl`, such
    /// as one with no offers, clears the cache.
    pub fn insert(&self, response: &Value) {
        self.insert_at(response, Instant::now());
    }

    /// Drops the cached response, for when the payer answers with an error.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    /// Marks the response stale when `rejection`, a `payer_sendTransaction`
    /// error's `data`, sends the wallet back to `payer_getTerms`: the payer
    /// paused, or rejected the payment without a `requote` to re-sign with. The
    /// next request then refetches. Ignored while a response refetched for that
    /// reason is fresh, so rejections refetch at most once per
    /// [`GET_TERMS_FRESH_FOR`].
    pub fn invalidate(&self, rejection: Option<&Value>) {
        self.invalidate_at(rejection, Instant::now());
    }

    /// Waits for any other refetch to finish, so concurrent misses share one
    /// upstream call.
    ///
    /// The guard holds when the last refetch failed to reach the payer. Callers
    /// that queued before that failure reuse it instead of repeating the call,
    /// so a slow payer delays each of them by one upstream timeout at most.
    pub async fn refreshing(&self) -> MutexGuard<'_, Option<Instant>> {
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

    fn get_at(&self, now: Instant) -> Option<Value> {
        let entry = self.lock();
        let entry = entry.as_ref().filter(|entry| entry.fresh(now) && !entry.stale)?;
        entry.served(now)
    }

    fn invalidate_at(&self, rejection: Option<&Value>, now: Instant) {
        if !rejection.is_some_and(Self::requires_new_terms) {
            return;
        }
        if let Some(entry) =
            self.lock().as_mut().filter(|entry| !(entry.forced && entry.fresh(now)))
        {
            entry.stale = true;
        }
    }

    fn requires_new_terms(rejection: &Value) -> bool {
        match rejection.get("code").and_then(Value::as_str) {
            Some(TEMPORARILY_UNAVAILABLE) => true,
            Some(PAYMENT_INSUFFICIENT) => rejection.get("requote").is_none_or(Value::is_null),
            _ => false,
        }
    }

    fn insert_at(&self, response: &Value, now: Instant) {
        let mut entry = self.lock();
        let Some(ttl_secs) = Self::ttl_secs(response) else {
            *entry = None;
            return;
        };
        let forced = entry.as_ref().is_some_and(|entry| entry.stale);
        *entry = Some(CachedTerms {
            response: response.clone(),
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
        json!({ "code": code, "reason": "rejected" })
    }

    #[test]
    fn serves_a_response_only_while_it_is_fresh() {
        let cache = TermsCache::new();
        let fetched_at = Instant::now();
        assert!(cache.get_at(fetched_at).is_none());

        cache.insert_at(&terms(60), fetched_at);

        let last_fresh = fetched_at + GET_TERMS_FRESH_FOR - Duration::from_millis(1);
        assert!(cache.get_at(last_fresh).is_some());
        assert!(cache.get_at(fetched_at + GET_TERMS_FRESH_FOR).is_none());
    }

    #[test]
    fn reduces_each_ttl_by_the_entry_age() {
        let cache = TermsCache::new();
        let fetched_at = Instant::now() - Duration::from_millis(45_500);
        cache.insert_at(&terms(60), fetched_at);

        let served = cache.get_stale().unwrap();

        assert_eq!(served["options"][0]["ttl"], 15);
        let expired = TermsCache::new();
        expired.insert_at(&terms(1), Instant::now() - Duration::from_secs(1));
        assert!(expired.get_stale().is_none());
    }

    #[test]
    fn serves_an_invalidated_response_only_as_stale() {
        let cache = TermsCache::new();
        cache.insert(&terms(60));
        cache.invalidate(Some(&rejection("PAYMENT_INSUFFICIENT")));

        assert!(cache.get().is_none());
        assert_eq!(cache.get_stale(), Some(terms(60)));
    }

    #[test]
    fn keeps_serving_after_a_rejection_with_a_requote() {
        let cache = TermsCache::new();
        cache.insert(&terms(60));
        let mut requoted = rejection("PAYMENT_INSUFFICIENT");
        requoted["requote"] = json!({ "token": "native", "paymentAmount": "0x2", "rate": "0x1" });

        cache.invalidate(Some(&requoted));

        assert!(cache.get().is_some());
    }

    #[test]
    fn clears_on_a_response_without_offers() {
        let cache = TermsCache::new();
        cache.insert(&terms(60));

        cache.insert(&json!({ "options": [] }));

        assert!(cache.get_stale().is_none());
    }

    #[test]
    fn skips_responses_without_a_positive_ttl() {
        let cache = TermsCache::new();
        cache.insert(&json!({ "options": [{ "kind": "token" }] }));
        cache.insert(&terms(0));

        assert!(cache.get_stale().is_none());
    }

    #[test]
    fn uses_the_shortest_offer_ttl_capped() {
        let cache = TermsCache::new();
        let response = json!({
            "options": [{ "kind": "token", "ttl": 60 }, { "kind": "token", "ttl": 15 }]
        });
        cache.insert_at(&response, Instant::now() - Duration::from_secs(15));
        assert!(cache.get_stale().is_none());

        let capped = Duration::from_secs(MAX_GET_TERMS_CACHE_SECS);
        cache.insert_at(&terms(3_600), Instant::now() - capped);
        assert!(cache.get_stale().is_none());
    }

    #[test]
    fn refetches_at_most_once_per_freshness_window_after_rejections() {
        let cache = TermsCache::new();
        let now = Instant::now();
        cache.insert_at(&terms(60), now);

        cache.invalidate_at(Some(&rejection("SENDER_BALANCE_INSUFFICIENT")), now);
        cache.invalidate_at(None, now);
        assert!(cache.get_at(now).is_some());

        cache.invalidate_at(Some(&rejection("PAYMENT_INSUFFICIENT")), now);
        assert!(cache.get_at(now).is_none());
        cache.insert_at(&terms(60), now);
        cache.invalidate_at(Some(&rejection("TEMPORARILY_UNAVAILABLE")), now);
        assert!(cache.get_at(now).is_some());

        let later = now + GET_TERMS_FRESH_FOR;
        cache.invalidate_at(Some(&rejection("PAYMENT_INSUFFICIENT")), later);
        assert!(cache.get_at(now).is_none());
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
