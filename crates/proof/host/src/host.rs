use std::sync::Arc;

use alloy_provider::{Network, RootProvider};
use base_alloy_evm::OpEvmFactory;
use base_alloy_network::Base;
use base_consensus_providers::{OnlineBeaconClient, OnlineBlobProvider};
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
use tracing::{Instrument, info, info_span};

#[cfg(feature = "disk")]
use crate::DiskKeyValueStore;
use crate::{
    BootKeyValueStore, HostConfig, HostError, HostProviders, MemoryKeyValueStore,
    OfflineHostBackend, OnlineHostBackend, PreimageServer, RecordingOracle, Result,
    SharedKeyValueStore, SplitKeyValueStore, metrics::timed,
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
        let kv_store = self.create_key_value_store()?;

        let task_handle = if self.config.is_offline() {
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
            let backend =
                OnlineHostBackend::new(self.config.clone(), Arc::clone(&kv_store), providers)
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
        let witness = Arc::new(witness);

        let kv_store = self.create_key_value_store()?;
        let providers = self.create_providers().await?;
        let backend = Arc::new(
            OnlineHostBackend::new(self.config.clone(), Arc::clone(&kv_store), providers)
                .with_proactive_hint(HintType::L2PayloadWitness),
        );

        let preimage_chan = BidirectionalChannel::new().map_err(HostError::Io)?;
        let hint_chan = BidirectionalChannel::new().map_err(HostError::Io)?;

        let server = PreimageServer::new(
            OracleServer::new(preimage_chan.host),
            HintReader::new(hint_chan.host),
            Arc::clone(&backend),
        );
        let mut server_task = task::spawn(
            async move { server.start().await }.instrument(info_span!("preimage_server")),
        );

        let recording = RecordingOracle::new(
            OracleReader::new(preimage_chan.client),
            HintWriter::new(hint_chan.client),
            Arc::clone(&witness),
        );

        let client_task = Box::pin(async {
            Self::run_client(recording).instrument(info_span!("run_client")).await
        });

        tokio::select! {
            result = &mut server_task => {
                return match result {
                    Err(e) => Err(HostError::ServerPanicked(e)),
                    Ok(Err(e)) => Err(e),
                    Ok(Ok(())) => Err(HostError::ServerExitedUnexpectedly),
                };
            }
            result = client_task => {
                result.map_err(|e| HostError::ProofProgram(Box::new(e)))?;
            }
        }

        server_task.abort();
        let _ = (&mut server_task).await;

        witness.finalize()?;
        let preimage_count = witness.preimage_count()?;
        base_metrics::set!(gauge, crate::Metrics::PREIMAGE_COUNT, preimage_count as f64);
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
        let _timer = timed!(crate::Metrics::REPLAY_DURATION_SECONDS);
        let driver =
            Prologue::new(recording.clone(), recording, OpEvmFactory::default()).load().await?;
        let epilogue = driver.execute().await?;
        epilogue.validate().map_err(|e| *e)?;
        Ok(())
    }

    /// Creates the key-value store for the host backend.
    pub fn create_key_value_store(&self) -> Result<SharedKeyValueStore> {
        let boot_kv = BootKeyValueStore::new(self.config.clone());

        let kv_store: SharedKeyValueStore = if let Some(ref data_dir) = self.config.data_dir {
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
        let l1_provider = rpc_provider(&self.config.prover.l1_eth_url).await?;
        let blob_provider = OnlineBlobProvider::init(OnlineBeaconClient::new_http(
            self.config.prover.l1_beacon_url.clone(),
        ))
        .await;
        let l2_provider = rpc_provider::<Base>(&self.config.prover.l2_eth_url).await?;

        Ok(HostProviders { l1: l1_provider, blobs: blob_provider, l2: l2_provider })
    }
}

async fn rpc_provider<N: Network>(url: &str) -> Result<RootProvider<N>> {
    RootProvider::connect(url)
        .await
        .map_err(|e| HostError::Custom(format!("failed to connect to RPC at {url}: {e}")))
}
