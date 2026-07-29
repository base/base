use std::sync::Arc;

use alloy_provider::{Network, RootProvider};
use base_common_evm::BaseEvmFactory;
use base_common_genesis::{L1TxFormat, RollupConfig};
use base_common_network::{Base, L1RpcProvider};
use base_consensus_derive::DynAltDaResolver;
use base_consensus_providers::{
    HttpAltDaResolver, L1BlobProvider, OnlineBeaconClient, OnlineBlobProvider,
};
use base_optimism_rpc::OptimismRollupProviderExt;
use base_proof::HintType;
use base_proof_client::{FaultProofProgramError, Prologue};
use base_proof_preimage::{
    BidirectionalChannel, Channel, HintReader, HintWriter, OracleReader, OracleServer,
    WitnessOracle,
};
use tokio::{
    sync::RwLock,
    task::{self, JoinHandle},
};
use tracing::{Instrument, info, info_span, warn};

#[cfg(feature = "disk")]
use crate::DiskKeyValueStore;
use crate::{
    BootKeyValueStore, HostConfig, HostError, HostProviders, MemoryKeyValueStore, Metrics,
    OfflineHostBackend, OnlineHostBackend, PreimageServer, RecordingOracle, Result,
    SharedKeyValueStore, SplitKeyValueStore, handler::L1HeaderCache,
};

/// The proof host orchestrator.
#[derive(Debug)]
pub struct Host {
    /// The host configuration.
    pub config: HostConfig,
}

impl Host {
    /// Creates a new [`Host`] from the given [`HostConfig`].
    pub const fn new(config: HostConfig) -> Self {
        Self { config }
    }

    /// Starts the preimage server, communicating with the client over the provided channels.
    pub async fn start_server<C>(&self, hint: C, preimage: C) -> Result<JoinHandle<Result<()>>>
    where
        C: Channel + Send + Sync + 'static,
    {
        let task_handle = if let Some(data_dir) = &self.config.data_dir {
            warn!(
                l2_chain_id = self.config.prover.l2_chain_id,
                data_dir = %data_dir.display(),
                "offline proof host will not refresh rollup config from L2 RPC; using configured rollup config"
            );
            let kv_store = self.create_key_value_store()?;
            task::spawn(async {
                PreimageServer::new(
                    OracleServer::new(preimage),
                    HintReader::new(hint),
                    Arc::new(OfflineHostBackend::new(kv_store)),
                )
                .start()
                .await
            })
        } else {
            let providers = self.create_providers().await?;
            let config = self.resolve_online_config(&providers).await?;
            let kv_store = Self::create_key_value_store_for_config(&config)?;
            let backend = OnlineHostBackend::new(config, Arc::clone(&kv_store), providers)
                .with_proactive_hint(HintType::L2PayloadWitness);

            task::spawn(async {
                PreimageServer::new(
                    OracleServer::new(preimage),
                    HintReader::new(hint),
                    Arc::new(backend),
                )
                .start()
                .await
            })
        };

        Ok(task_handle)
    }

    /// Runs the fault-proof program in-process, capturing all fetched preimages into the
    /// provided [`WitnessOracle`].
    ///
    /// Takes ownership of the oracle and returns it after witness generation completes.
    /// [`Arc`] sharing with internal tasks is managed entirely within this method.
    pub async fn build_witness<W>(&self, witness: W) -> Result<W>
    where
        W: WitnessOracle + std::fmt::Debug + 'static,
    {
        self.build_witness_with_l1_header_cache(witness, L1HeaderCache::new()).await
    }

    pub(crate) async fn build_witness_with_l1_header_cache<W>(
        &self,
        witness: W,
        l1_header_cache: L1HeaderCache,
    ) -> Result<W>
    where
        W: WitnessOracle + std::fmt::Debug + 'static,
    {
        let witness = Arc::new(witness);

        let providers = self.create_providers().await?;
        let config = self.resolve_online_config(&providers).await?;
        let kv_store = Self::create_key_value_store_for_config(&config)?;
        let backend = Arc::new(
            OnlineHostBackend::new_with_l1_header_cache(
                config,
                Arc::clone(&kv_store),
                providers,
                l1_header_cache,
            )
            .with_proactive_hint(HintType::L2PayloadWitness),
        );

        let preimage_chan = BidirectionalChannel::new().map_err(HostError::Io)?;
        let hint_chan = BidirectionalChannel::new().map_err(HostError::Io)?;

        let server = PreimageServer::new(
            OracleServer::new(preimage_chan.host),
            HintReader::new(hint_chan.host),
            Arc::clone(&backend),
        );
        let mut tasks = task::JoinSet::new();
        tasks.spawn(async move { server.start().await }.instrument(info_span!("preimage_server")));

        let recording = RecordingOracle::new(
            OracleReader::new(preimage_chan.client),
            HintWriter::new(hint_chan.client),
            Arc::clone(&witness),
        );

        let client_task = Box::pin(async {
            Self::run_client(recording).instrument(info_span!("run_client")).await
        });

        tokio::select! {
            result = tasks.join_next() => {
                return match result {
                    Some(Err(e)) => Err(HostError::ServerPanicked(e)),
                    Some(Ok(Err(e))) => Err(e),
                    Some(Ok(Ok(()))) | None => Err(HostError::ServerExitedUnexpectedly),
                };
            }
            result = client_task => {
                result.map_err(|e| HostError::ProofProgram(Box::new(e)))?;
            }
        }

        witness.finalize()?;
        let preimage_count = witness.preimage_count()?;
        Metrics::preimage_count().set(preimage_count as f64);
        info!(preimage_count, "witness capture complete");

        Arc::try_unwrap(witness).map_err(|arc| {
            HostError::Custom(format!(
                "failed to recover witness oracle: {} references still held",
                Arc::strong_count(&arc),
            ))
        })
    }

    /// Runs the fault-proof program client: prologue → driver → epilogue.
    async fn run_client<P, H, W>(
        recording: RecordingOracle<P, H, W>,
    ) -> std::result::Result<(), FaultProofProgramError>
    where
        P: base_proof_preimage::PreimageOracleClient
            + Send
            + Sync
            + Clone
            + std::fmt::Debug
            + 'static,
        H: base_proof_preimage::HintWriterClient + Send + Sync + Clone + std::fmt::Debug + 'static,
        W: WitnessOracle + std::fmt::Debug + 'static,
    {
        let _timer = base_metrics::timed!(Metrics::replay_duration_seconds());
        let driver =
            Prologue::new(recording.clone(), recording, BaseEvmFactory::default()).load().await?;
        let epilogue = driver.execute().await?;
        epilogue.validate().map_err(|e| *e)?;
        Ok(())
    }

    /// Creates the key-value store for the host backend.
    pub fn create_key_value_store(&self) -> Result<SharedKeyValueStore> {
        Self::create_key_value_store_for_config(&self.config)
    }

    async fn resolve_online_config(&self, providers: &HostProviders) -> Result<HostConfig> {
        let rollup_config: RollupConfig =
            serde_json::from_value(providers.l2_node.optimism_rollup_config().await?)?;

        // Fail fast if the L2 RPC serves a config for the wrong chain; the guest re-checks this,
        // but a host-side error is clearer and avoids wasted work.
        let rpc_chain_id = rollup_config.l2_chain_id.id();
        if rpc_chain_id != self.config.prover.l2_chain_id {
            return Err(HostError::Custom(format!(
                "L2 RPC returned rollup config for chain {rpc_chain_id} but host is configured for chain {}",
                self.config.prover.l2_chain_id,
            )));
        }

        let mut config = self.config.clone();
        config.prover.rollup_config = rollup_config;
        Ok(config)
    }

    fn create_key_value_store_for_config(config: &HostConfig) -> Result<SharedKeyValueStore> {
        let boot_kv = BootKeyValueStore::new(config.clone());

        let kv_store: SharedKeyValueStore = if let Some(ref data_dir) = config.data_dir {
            #[cfg(feature = "disk")]
            {
                let disk_kv_store = DiskKeyValueStore::new(data_dir.clone());
                let split_kv_store = SplitKeyValueStore::new(boot_kv, disk_kv_store);
                Arc::new(RwLock::new(split_kv_store))
            }
            #[cfg(not(feature = "disk"))]
            {
                let _ = data_dir;
                let mem_kv_store = MemoryKeyValueStore::new();
                let split_kv_store = SplitKeyValueStore::new(boot_kv, mem_kv_store);
                Arc::new(RwLock::new(split_kv_store))
            }
        } else {
            let mem_kv_store = MemoryKeyValueStore::new();
            let split_kv_store = SplitKeyValueStore::new(boot_kv, mem_kv_store);
            Arc::new(RwLock::new(split_kv_store))
        };

        Ok(kv_store)
    }

    /// Creates the providers required for the host backend.
    pub async fn create_providers(&self) -> Result<HostProviders> {
        let l1_tx_format = L1TxFormat::from_l1_config(&self.config.prover.l1_config);
        let l1_beacon_url = self
            .config
            .prover
            .l1_beacon_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty());

        if l1_tx_format == L1TxFormat::Base && l1_beacon_url.is_some() {
            return Err(HostError::Custom(
                "Base-format L1 config has no beacon/blob DA; run with calldata-only L1 derivation"
                    .into(),
            ));
        }

        let l1_provider = match l1_tx_format {
            L1TxFormat::Ethereum => {
                L1RpcProvider::Ethereum(rpc_provider(&self.config.prover.l1_eth_url).await?)
            }
            L1TxFormat::Base => {
                L1RpcProvider::Base(rpc_provider::<Base>(&self.config.prover.l1_eth_url).await?)
            }
        };
        let blob_provider = match l1_beacon_url {
            Some(url) => L1BlobProvider::beacon(
                OnlineBlobProvider::init(OnlineBeaconClient::new_http(url.to_string())).await,
            ),
            None => L1BlobProvider::calldata_only(),
        };
        let l2_provider = rpc_provider::<Base>(&self.config.prover.l2_eth_url).await?;
        let l2_node_provider = rpc_provider(&self.config.prover.l2_node_url).await?;

        let alt_da = match self
            .config
            .prover
            .da_server_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            Some(url) => {
                let parsed = url.parse().map_err(|error| {
                    HostError::Custom(format!("invalid da-server url {url:?}: {error}"))
                })?;
                let resolver = HttpAltDaResolver::new(parsed).map_err(|error| {
                    HostError::Custom(format!("da-server client init failed: {error}"))
                })?;
                Some(Arc::new(resolver) as DynAltDaResolver)
            }
            None => None,
        };

        Ok(HostProviders {
            l1: l1_provider,
            blobs: blob_provider,
            l2: l2_provider,
            l2_node: l2_node_provider,
            alt_da,
        })
    }
}

async fn rpc_provider<N: Network>(url: &str) -> Result<RootProvider<N>> {
    RootProvider::connect(url)
        .await
        .map_err(|e| HostError::Custom(format!("failed to connect to RPC at {url}: {e}")))
}

#[cfg(test)]
mod tests {
    use alloy_genesis::ChainConfig;
    use base_common_genesis::RollupConfig;
    use base_proof_primitives::ProofRequest;
    use serde_json::json;

    use super::*;

    fn test_config(l1_config: ChainConfig, l1_beacon_url: Option<String>) -> HostConfig {
        HostConfig {
            request: ProofRequest::default(),
            prover: crate::ProverConfig {
                l1_eth_url: "http://127.0.0.1:1".to_string(),
                l2_eth_url: "http://127.0.0.1:1".to_string(),
                l2_node_url: "http://127.0.0.1:1".to_string(),
                l1_beacon_url,
                l2_chain_id: 0,
                rollup_config: RollupConfig::default(),
                l1_config,
                enable_experimental_witness_endpoint: false,
                da_server_url: None,
            },
            data_dir: None,
        }
    }

    #[tokio::test]
    async fn base_l1_config_rejects_beacon_provider() {
        let mut l1_config = ChainConfig::default();
        l1_config.extra_fields.insert("optimism".to_string(), json!({}));
        let host = Host::new(test_config(l1_config, Some("http://127.0.0.1:1".to_string())));

        let err = host.create_providers().await.expect_err("Base beacon mode should reject");

        assert!(matches!(err, HostError::Custom(message) if message.contains("calldata-only")));
    }
}
