use std::{collections::HashSet, fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use base_proof::{Hint, HintType};
use base_proof_preimage::{
    HintRouter, PreimageFetcher, PreimageKey,
    errors::{PreimageOracleError, PreimageOracleResult},
};
use tokio::{sync::RwLock, time};
use tracing::{debug, error, trace, warn};

use crate::{
    HostConfig, HostProviders, Metrics, SharedKeyValueStore,
    handler::{
        L1HeaderCache, L1HeaderPrefetcher, PayloadWitnessPrefetcher, handle_hint_with_prefetchers,
    },
};

/// Cap on `handle_hint` attempts before [`OnlineHostBackend::get_preimage`] gives up; previously
/// the loop was unbounded and could pin a [`PreimageServer`](crate::PreimageServer) task forever
/// when an upstream RPC consistently failed.
const MAX_HINT_ATTEMPTS: u32 = 5;

/// Initial backoff between hint retry attempts; doubles each attempt up to [`MAX_HINT_RETRY_BACKOFF`].
const INITIAL_HINT_RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// Cap on per-attempt backoff between hint retries.
const MAX_HINT_RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// Fetches data from remote sources in response to hints.
pub struct OnlineHostBackend {
    cfg: Arc<HostConfig>,
    kv: SharedKeyValueStore,
    providers: Arc<HostProviders>,
    proactive_hints: HashSet<HintType>,
    last_hint: Arc<RwLock<Option<Hint<HintType>>>>,
    payload_witness_prefetcher: PayloadWitnessPrefetcher,
    l1_header_prefetcher: L1HeaderPrefetcher,
}

impl fmt::Debug for OnlineHostBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnlineHostBackend").finish_non_exhaustive()
    }
}

impl OnlineHostBackend {
    /// Creates a new [`OnlineHostBackend`].
    pub fn new(cfg: HostConfig, kv: SharedKeyValueStore, providers: HostProviders) -> Self {
        Self::new_with_l1_header_cache(cfg, kv, providers, L1HeaderCache::new())
    }

    /// Creates a new [`OnlineHostBackend`] with a shared L1 header cache.
    pub(crate) fn new_with_l1_header_cache(
        cfg: HostConfig,
        kv: SharedKeyValueStore,
        providers: HostProviders,
        l1_header_cache: L1HeaderCache,
    ) -> Self {
        let cfg = Arc::new(cfg);
        let providers = Arc::new(providers);
        let payload_witness_prefetcher =
            PayloadWitnessPrefetcher::new(Arc::clone(&cfg), Arc::clone(&providers));
        let l1_header_prefetcher = L1HeaderPrefetcher::new(Arc::clone(&providers), l1_header_cache);

        Self {
            cfg,
            kv,
            providers,
            proactive_hints: HashSet::default(),
            last_hint: Arc::new(RwLock::new(None)),
            payload_witness_prefetcher,
            l1_header_prefetcher,
        }
    }

    /// Adds a proactive hint type that is immediately fetched upon receipt.
    pub fn with_proactive_hint(mut self, hint_type: HintType) -> Self {
        self.proactive_hints.insert(hint_type);
        self
    }
}

#[async_trait]
impl HintRouter for OnlineHostBackend {
    async fn route_hint(&self, hint: String) -> PreimageOracleResult<()> {
        trace!(target: "host_backend", raw_hint = %hint, "received hint");

        let parsed_hint = hint
            .parse::<Hint<HintType>>()
            .map_err(|e| PreimageOracleError::HintParseFailed(e.to_string()))?;
        if self.proactive_hints.contains(&parsed_hint.ty) {
            debug!(target: "host_backend", raw_hint = %hint, "proactive hint received, immediately fetching");
            handle_hint_with_prefetchers(
                parsed_hint,
                &self.cfg,
                &self.providers,
                Arc::clone(&self.kv),
                Some(self.payload_witness_prefetcher.clone()),
                Some(self.l1_header_prefetcher.clone()),
            )
            .await
            .map_err(|e| PreimageOracleError::Other(e.to_string()))?;
        } else {
            let mut hint_lock = self.last_hint.write().await;
            hint_lock.replace(parsed_hint);
        }

        Ok(())
    }
}

#[async_trait]
impl PreimageFetcher for OnlineHostBackend {
    async fn get_preimage(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        trace!(target: "host_backend", preimage_key = %key, "preimage requested");

        let kv_lock = self.kv.read().await;
        let preimage = kv_lock.get(key.into());
        drop(kv_lock);

        if let Some(data) = preimage {
            return Ok(data);
        }

        Metrics::kv_cold_lookups_total().increment(1);

        // Snapshot the hint once, so failures observe a stable target rather than racing with the
        // hint writer.
        let Some(hint) = self.last_hint.read().await.clone() else {
            warn!(
                target: "host_backend",
                preimage_key = %key,
                "preimage missing from KV with no pending hint to fetch it",
            );
            return Err(PreimageOracleError::Other(format!(
                "preimage {key} missing from KV and no hint has been routed",
            )));
        };

        let mut backoff = INITIAL_HINT_RETRY_BACKOFF;
        let mut last_error: Option<String> = None;
        for attempt in 1..=MAX_HINT_ATTEMPTS {
            match handle_hint_with_prefetchers(
                hint.clone(),
                &self.cfg,
                &self.providers,
                Arc::clone(&self.kv),
                Some(self.payload_witness_prefetcher.clone()),
                Some(self.l1_header_prefetcher.clone()),
            )
            .await
            {
                Ok(()) => {
                    let kv_lock = self.kv.read().await;
                    let preimage = kv_lock.get(key.into());
                    drop(kv_lock);

                    if let Some(data) = preimage {
                        return Ok(data);
                    }

                    // The hint completed but did not populate the requested key. Retrying the
                    // same hint will not produce a different result, so fail fast.
                    warn!(
                        target: "host_backend",
                        preimage_key = %key,
                        hint = ?hint,
                        attempt,
                        "hint succeeded but did not populate requested preimage",
                    );
                    return Err(PreimageOracleError::KeyNotFound);
                }
                Err(e) => {
                    let message = e.to_string();
                    error!(
                        target: "host_backend",
                        error = %e,
                        hint = ?hint,
                        attempt,
                        max_attempts = MAX_HINT_ATTEMPTS,
                        "failed to prefetch hint",
                    );
                    last_error = Some(message);

                    if attempt < MAX_HINT_ATTEMPTS {
                        time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_HINT_RETRY_BACKOFF);
                    }
                }
            }
        }

        let last_error = last_error.unwrap_or_else(|| "unknown error".to_string());
        Err(PreimageOracleError::Other(format!(
            "exhausted {MAX_HINT_ATTEMPTS} hint attempts for preimage {key}: {last_error}",
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_genesis::ChainConfig;
    use alloy_provider::RootProvider;
    use base_common_genesis::RollupConfig;
    use base_consensus_providers::{OnlineBeaconClient, OnlineBlobProvider};
    use base_proof::{Hint, HintType};
    use base_proof_preimage::{
        HintRouter, PreimageFetcher, PreimageKey, errors::PreimageOracleError,
    };
    use base_proof_primitives::ProofRequest;
    use tokio::sync::RwLock;

    use super::*;
    use crate::{
        HostConfig, HostProviders, MemoryKeyValueStore, ProverConfig, SharedKeyValueStore,
    };

    fn test_kv() -> SharedKeyValueStore {
        Arc::new(RwLock::new(MemoryKeyValueStore::new()))
    }

    fn test_providers() -> HostProviders {
        // The tests below trigger `handle_hint` failures *before* any RPC call (via invalid hint
        // payloads), so the providers never need to dial the unreachable endpoints.
        let l1 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        let l2 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        let beacon = OnlineBeaconClient::new_http("http://127.0.0.1:1".to_string());
        let blobs =
            OnlineBlobProvider { beacon_client: beacon, genesis_time: 0, slot_interval: 12 };
        HostProviders { l1, blobs, l2 }
    }

    fn test_cfg() -> HostConfig {
        HostConfig {
            request: ProofRequest::default(),
            prover: ProverConfig {
                l1_eth_url: "http://127.0.0.1:1".to_string(),
                l2_eth_url: "http://127.0.0.1:1".to_string(),
                l1_beacon_url: "http://127.0.0.1:1".to_string(),
                l2_chain_id: 0,
                rollup_config: RollupConfig::default(),
                l1_config: ChainConfig::default(),
                enable_experimental_witness_endpoint: false,
            },
            data_dir: None,
        }
    }

    fn unknown_key() -> PreimageKey {
        PreimageKey::new_keccak256([0xAB; 32])
    }

    /// Path B: no hint has been routed and the key is not in the KV. The backend must fail fast
    /// rather than spin on `last_hint.read()`.
    #[tokio::test]
    async fn returns_error_when_no_hint_and_key_missing() {
        let backend = OnlineHostBackend::new(test_cfg(), test_kv(), test_providers());

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            backend.get_preimage(unknown_key()),
        )
        .await
        .expect("get_preimage should not hang when no hint is set");

        match result {
            Err(PreimageOracleError::Other(msg)) => {
                assert!(msg.contains("no hint"), "unexpected error message: {msg}");
            }
            other => panic!("expected Other error, got {other:?}"),
        }
    }

    /// Path A: a hint is routed but `handle_hint` fails on every attempt. The backend must give
    /// up after `MAX_HINT_ATTEMPTS` and propagate an error rather than retrying forever.
    ///
    /// `L1BlockHeader` with empty data triggers `HostError::InvalidHintDataLength` *before* any
    /// RPC call, giving us a deterministic, repeated failure to exercise the retry budget.
    #[tokio::test]
    async fn returns_error_after_exhausting_hint_retries() {
        let backend = OnlineHostBackend::new(test_cfg(), test_kv(), test_providers());

        // Route a hint whose handler always errors deterministically.
        let bad_hint = Hint::new(HintType::L1BlockHeader, Vec::<u8>::new());
        backend.route_hint(bad_hint.encode()).await.expect("route_hint should accept the hint");

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            backend.get_preimage(unknown_key()),
        )
        .await
        .expect("get_preimage must terminate after exhausting retries");

        match result {
            Err(PreimageOracleError::Other(msg)) => {
                assert!(msg.contains("exhausted"), "unexpected error message: {msg}");
                assert!(
                    msg.contains(&MAX_HINT_ATTEMPTS.to_string()),
                    "error should mention attempt count: {msg}",
                );
            }
            other => panic!("expected Other error after retries, got {other:?}"),
        }
    }
}
