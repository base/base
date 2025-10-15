use std::{fmt::Debug, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use hokulea_eigenda::{EigenDADataSource, EigenDAPreimageProvider, EigenDAPreimageSource};
use kona_derive::{BlobProvider, EthereumDataSource};
use kona_driver::PipelineCursor;
use kona_genesis::{L1ChainConfig, RollupConfig};
use kona_preimage::CommsClient;
use kona_proof::{
    l1::{OracleL1ChainProvider, OraclePipeline},
    l2::OracleL2ChainProvider,
    FlushableCache,
};
use op_succinct_client_utils::witness::executor::WitnessExecutor;
use spin::RwLock;

pub struct EigenDAWitnessExecutor<O, B, E>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    B: BlobProvider + Send + Sync + Debug + Clone,
    E: EigenDAPreimageProvider + Send + Sync + Debug + Clone,
{
    eigenda_preimage_provider: E,
    _marker: std::marker::PhantomData<(O, B)>,
}

impl<O, B, E> EigenDAWitnessExecutor<O, B, E>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    B: BlobProvider + Send + Sync + Debug + Clone,
    E: EigenDAPreimageProvider + Send + Sync + Debug + Clone,
{
    pub fn new(eigenda_preimage_provider: E) -> Self {
        Self { eigenda_preimage_provider, _marker: std::marker::PhantomData }
    }
}

#[async_trait]
impl<O, B, E> WitnessExecutor for EigenDAWitnessExecutor<O, B, E>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    B: BlobProvider + Send + Sync + Debug + Clone,
    E: EigenDAPreimageProvider + Send + Sync + Debug + Clone,
{
    type O = O;
    type B = B;
    type L1 = OracleL1ChainProvider<Self::O>;
    type L2 = OracleL2ChainProvider<Self::O>;
    type DA = EigenDADataSource<Self::L1, Self::B, E>;

    async fn create_pipeline(
        &self,
        rollup_config: Arc<RollupConfig>,
        l1_config: Arc<L1ChainConfig>,
        cursor: Arc<RwLock<PipelineCursor>>,
        oracle: Arc<Self::O>,
        beacon: Self::B,
        l1_provider: Self::L1,
        l2_provider: Self::L2,
    ) -> Result<OraclePipeline<Self::O, Self::L1, Self::L2, Self::DA>> {
        let ethereum_data_source =
            EthereumDataSource::new_from_parts(l1_provider.clone(), beacon, &rollup_config);
        let eigenda_preimage_source =
            EigenDAPreimageSource::new(self.eigenda_preimage_provider.clone());
        let da_provider = EigenDADataSource::new(ethereum_data_source, eigenda_preimage_source);

        Ok(OraclePipeline::new(
            rollup_config,
            l1_config,
            cursor,
            oracle,
            da_provider,
            l1_provider,
            l2_provider,
        )
        .await?)
    }
}
