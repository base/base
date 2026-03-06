use std::{fmt::Debug, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use hana_celestia::{CelestiaDADataSource, CelestiaDASource};
use hana_oracle::provider::OracleCelestiaProvider;
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

pub struct CelestiaDAWitnessExecutor<O, B>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    B: BlobProvider + Send + Sync + Debug + Clone,
{
    _marker: std::marker::PhantomData<(O, B)>,
}

#[allow(clippy::new_without_default)]
impl<O, B> CelestiaDAWitnessExecutor<O, B>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    B: BlobProvider + Send + Sync + Debug + Clone,
{
    pub fn new() -> Self {
        Self { _marker: std::marker::PhantomData }
    }
}

#[async_trait]
impl<O, B> WitnessExecutor for CelestiaDAWitnessExecutor<O, B>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    B: BlobProvider + Send + Sync + Debug + Clone,
{
    type O = O;
    type B = B;
    type L1 = OracleL1ChainProvider<Self::O>;
    type L2 = OracleL2ChainProvider<Self::O>;
    type DA = CelestiaDADataSource<Self::L1, Self::B, OracleCelestiaProvider<Self::O>>;

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
        let celestia_data_source =
            CelestiaDASource::new(OracleCelestiaProvider::new(oracle.clone()));
        let da_provider = CelestiaDADataSource::new(ethereum_data_source, celestia_data_source);

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
