use std::fmt::Debug;

use anyhow::Result;
use async_trait::async_trait;
use base_consensus_derive::{BlobProvider, EthereumDataSource};
use base_proof::{OracleL1ChainProvider, OracleL2ChainProvider, OraclePipeline};
use base_proof_preimage::{CommsClient, FlushableCache};
use base_proof_succinct_client_utils::{WitnessExecutor, WitnessPipelineParts};

/// Witness executor for Ethereum data availability.
#[derive(Debug)]
pub struct ETHDAWitnessExecutor<O, B>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    B: BlobProvider + Send + Sync + Debug + Clone,
{
    _marker: std::marker::PhantomData<(O, B)>,
}

impl<O, B> ETHDAWitnessExecutor<O, B>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    B: BlobProvider + Send + Sync + Debug + Clone,
{
    /// Create a new executor.
    pub const fn new() -> Self {
        Self { _marker: std::marker::PhantomData }
    }
}

impl<O, B> Default for ETHDAWitnessExecutor<O, B>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    B: BlobProvider + Send + Sync + Debug + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl<O, B> WitnessExecutor for ETHDAWitnessExecutor<O, B>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    B: BlobProvider + Send + Sync + Debug + Clone,
{
    type O = O;
    type B = B;
    type L1 = OracleL1ChainProvider<Self::O>;
    type L2 = OracleL2ChainProvider<Self::O>;
    type DA = EthereumDataSource<Self::L1, Self::B>;

    async fn create_pipeline(
        &self,
        parts: WitnessPipelineParts<Self::O, Self::B, Self::L1, Self::L2>,
    ) -> Result<OraclePipeline<Self::O, Self::L1, Self::L2, Self::DA>> {
        let da_provider = EthereumDataSource::new_from_parts(
            parts.l1_provider.clone(),
            parts.beacon,
            &parts.rollup_config,
        );
        Ok(OraclePipeline::new(
            parts.rollup_config,
            parts.l1_config,
            parts.cursor,
            parts.oracle,
            da_provider,
            parts.l1_provider,
            parts.l2_provider,
        )
        .await?)
    }
}
