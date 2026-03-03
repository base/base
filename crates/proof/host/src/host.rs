//! Host runtime.

use std::sync::Arc;

use base_alloy_network::Base;
use base_consensus_providers::{OnlineBeaconClient, OnlineBlobProvider};
use base_proof::HintType;
use base_proof_preimage::{BidirectionalChannel, Channel, HintReader, OracleServer};
use base_proof_std_fpvm::{FileChannel, FileDescriptor};
use tokio::{
    sync::RwLock,
    task::{self, JoinHandle},
};

use crate::{
    DiskKeyValueStore, HostArgs, HostError, HostProviders, LocalInputs, MemoryKeyValueStore,
    OfflineHostBackend, OnlineHostBackend, PreimageServer, RpcProviderFactory, SharedKeyValueStore,
    SplitKeyValueStore,
};

/// The preimage oracle host runtime.
#[derive(Clone, Debug)]
pub struct Host {
    /// The host arguments.
    pub args: HostArgs,
}

impl Host {
    /// Creates a new [`Host`] with the given arguments.
    pub const fn new(args: HostArgs) -> Self {
        Self { args }
    }

    /// Starts the [`Host`].
    pub async fn start(self) -> Result<(), HostError> {
        if self.args.server {
            let hint = FileChannel::new(FileDescriptor::HintRead, FileDescriptor::HintWrite);
            let preimage =
                FileChannel::new(FileDescriptor::PreimageRead, FileDescriptor::PreimageWrite);

            self.start_server(hint, preimage).await?.await?
        } else {
            self.start_native().await
        }
    }

    /// Starts the preimage server, communicating with the client over the provided channels.
    pub async fn start_server<C>(
        &self,
        hint: C,
        preimage: C,
    ) -> Result<JoinHandle<Result<(), HostError>>, HostError>
    where
        C: Channel + Send + Sync + 'static,
    {
        let kv_store = self.create_key_value_store()?;

        let task_handle = if self.args.is_offline() {
            task::spawn(async {
                PreimageServer::new(
                    OracleServer::new(preimage),
                    HintReader::new(hint),
                    Arc::new(OfflineHostBackend::new(kv_store)),
                )
                .start()
                .await
                .map_err(HostError::from)
            })
        } else {
            let providers = self.create_providers().await?;
            let backend =
                OnlineHostBackend::new(self.args.clone(), Arc::clone(&kv_store), providers)
                    .with_proactive_hint(HintType::L2PayloadWitness);

            task::spawn(async {
                PreimageServer::new(
                    OracleServer::new(preimage),
                    HintReader::new(hint),
                    Arc::new(backend),
                )
                .start()
                .await
                .map_err(HostError::from)
            })
        };

        Ok(task_handle)
    }

    async fn start_native(&self) -> Result<(), HostError> {
        let hint = BidirectionalChannel::new()?;
        let preimage = BidirectionalChannel::new()?;

        let server_task = self.start_server(hint.host, preimage.host).await?;
        let client_task = task::spawn(async { Ok::<(), HostError>(()) });

        let (_, _client_result) = tokio::try_join!(server_task, client_task)?;

        Ok(())
    }

    /// Creates the key-value store for the host backend.
    pub fn create_key_value_store(&self) -> Result<SharedKeyValueStore, HostError> {
        let local_kv_store = LocalInputs::new(self.args.clone());

        let kv_store: SharedKeyValueStore = if let Some(ref data_dir) = self.args.data_dir {
            let disk_kv_store = DiskKeyValueStore::new(data_dir.clone());
            let split_kv_store = SplitKeyValueStore::new(local_kv_store, disk_kv_store);
            Arc::new(RwLock::new(split_kv_store))
        } else {
            let mem_kv_store = MemoryKeyValueStore::new();
            let split_kv_store = SplitKeyValueStore::new(local_kv_store, mem_kv_store);
            Arc::new(RwLock::new(split_kv_store))
        };

        Ok(kv_store)
    }

    /// Creates the providers required for the host backend.
    pub async fn create_providers(&self) -> Result<HostProviders, HostError> {
        let l1_provider = RpcProviderFactory::connect(
            self.args.l1_node_address.as_ref().ok_or(HostError::Other("Provider must be set"))?,
        )
        .await?;
        let blob_provider = OnlineBlobProvider::init(OnlineBeaconClient::new_http(
            self.args
                .l1_beacon_address
                .clone()
                .ok_or(HostError::Other("Beacon API URL must be set"))?,
        ))
        .await;
        let l2_provider = RpcProviderFactory::connect::<Base>(
            self.args
                .l2_node_address
                .as_ref()
                .ok_or(HostError::Other("L2 node address must be set"))?,
        )
        .await?;

        Ok(HostProviders { l1: l1_provider, blobs: blob_provider, l2: l2_provider })
    }
}
