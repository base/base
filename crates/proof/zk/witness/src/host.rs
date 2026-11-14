use std::{fmt, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use base_proof_host::{Host, HostConfig, HostError};
use base_proof_preimage::{BidirectionalChannel, Channel};
use tokio::task::JoinHandle;

use crate::{
    fetcher::OPSuccinctDataFetcher,
    witness_generation::{DefaultWitnessData, WitnessGenerator},
};

/// Starts a preimage hint/oracle server.
#[async_trait]
pub trait PreimageServerStarter {
    /// Launch the server on the given hint and preimage channels.
    async fn start_server<C>(
        &self,
        hint: C,
        preimage: C,
    ) -> Result<JoinHandle<Result<(), HostError>>, HostError>
    where
        C: Channel + Send + Sync + 'static;
}

#[async_trait]
impl PreimageServerStarter for HostConfig {
    async fn start_server<C>(
        &self,
        hint: C,
        preimage: C,
    ) -> Result<JoinHandle<Result<(), HostError>>, HostError>
    where
        C: Channel + Send + Sync + 'static,
    {
        Host::new(self.clone()).start_server(hint, preimage).await
    }
}

/// Host for Succinct proof generation with Ethereum data availability.
#[derive(Clone)]
pub struct SuccinctHost {
    /// L1/L2 data fetcher.
    pub fetcher: Arc<OPSuccinctDataFetcher>,
    witness_generator: Arc<WitnessGenerator>,
}

impl fmt::Debug for SuccinctHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuccinctHost").finish_non_exhaustive()
    }
}

impl SuccinctHost {
    /// Create a new host from a data fetcher.
    pub fn new(fetcher: Arc<OPSuccinctDataFetcher>) -> Self {
        Self { fetcher, witness_generator: Arc::new(WitnessGenerator::new()) }
    }

    /// Run the host and client program.
    ///
    /// Returns the witness which can be supplied to the zkVM.
    pub async fn run(&self, args: &HostConfig) -> Result<DefaultWitnessData> {
        let preimage = BidirectionalChannel::new()?;
        let hint = BidirectionalChannel::new()?;

        let server_task = args.start_server(hint.host, preimage.host).await?;

        let witness = self.witness_generator.run(preimage.client, hint.client).await;
        // Unlike the upstream, manually abort the server task, as it will hang if you wait for both
        // tasks to complete.
        server_task.abort();

        witness
    }
}
