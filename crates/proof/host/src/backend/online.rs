//! Contains the [`OnlineHostBackend`] definition.

use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use base_proof::{Hint, HintType};
use base_proof_preimage::{
    HintRouter, PreimageFetcher, PreimageKey,
    errors::{PreimageOracleError, PreimageOracleResult},
};
use tokio::sync::RwLock;
use tracing::{debug, error, trace};

use crate::{HostArgs, HostProviders, SharedKeyValueStore, handler};

/// The [`OnlineHostBackend`] fetches data from remote sources in response to hints.
pub struct OnlineHostBackend {
    cfg: HostArgs,
    kv: SharedKeyValueStore,
    providers: HostProviders,
    proactive_hints: HashSet<HintType>,
    last_hint: Arc<RwLock<Option<Hint<HintType>>>>,
}

impl std::fmt::Debug for OnlineHostBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnlineHostBackend").finish_non_exhaustive()
    }
}

impl OnlineHostBackend {
    /// Creates a new [`OnlineHostBackend`].
    pub fn new(cfg: HostArgs, kv: SharedKeyValueStore, providers: HostProviders) -> Self {
        Self {
            cfg,
            kv,
            providers,
            proactive_hints: HashSet::default(),
            last_hint: Arc::new(RwLock::new(None)),
        }
    }

    /// Adds a proactive hint that will be immediately fetched upon receipt.
    pub fn with_proactive_hint(mut self, hint_type: HintType) -> Self {
        self.proactive_hints.insert(hint_type);
        self
    }

    async fn fetch(&self, hint: &Hint<HintType>) -> anyhow::Result<()> {
        handler::fetch_hint(hint.clone(), &self.cfg, &self.providers, Arc::clone(&self.kv)).await
    }
}

#[async_trait]
impl HintRouter for OnlineHostBackend {
    async fn route_hint(&self, hint: String) -> PreimageOracleResult<()> {
        trace!(target: "host_backend", hint = %hint, "received hint");

        let parsed_hint = hint
            .parse::<Hint<HintType>>()
            .map_err(|e| PreimageOracleError::HintParseFailed(e.to_string()))?;
        if self.proactive_hints.contains(&parsed_hint.ty) {
            debug!(target: "host_backend", hint = %hint, "proactive hint received, immediately fetching");
            self.fetch(&parsed_hint)
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
        trace!(target: "host_backend", key = %key, "pre-image requested");

        let kv_lock = self.kv.read().await;
        let mut preimage = kv_lock.get(key.into());
        drop(kv_lock);

        while preimage.is_none() {
            if let Some(hint) = self.last_hint.read().await.as_ref() {
                let value = self.fetch(hint).await;

                if let Err(e) = value {
                    error!(target: "host_backend", error = %e, "failed to prefetch hint");
                    continue;
                }

                let kv_lock = self.kv.read().await;
                preimage = kv_lock.get(key.into());
            }
        }

        preimage.ok_or(PreimageOracleError::KeyNotFound)
    }
}
