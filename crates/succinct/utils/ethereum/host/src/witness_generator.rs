use anyhow::Result;
use async_trait::async_trait;
use base_proof::OracleBlobProvider;
use base_succinct_client_utils::witness::DefaultWitnessData;
use base_succinct_ethereum_client_utils::executor::ETHDAWitnessExecutor;
use base_succinct_host_utils::witness_generation::{
    DefaultOracleBase, WitnessGenerator, online_blob_store::OnlineBlobStore,
    preimage_witness_collector::PreimageWitnessCollector,
};
use rkyv::to_bytes;
use sp1_sdk::SP1Stdin;

type WitnessExecutor = ETHDAWitnessExecutor<
    PreimageWitnessCollector<DefaultOracleBase>,
    OnlineBlobStore<OracleBlobProvider<DefaultOracleBase>>,
>;

pub struct ETHDAWitnessGenerator {
    pub executor: WitnessExecutor,
}

#[async_trait]
impl WitnessGenerator for ETHDAWitnessGenerator {
    type WitnessData = DefaultWitnessData;
    type WitnessExecutor = WitnessExecutor;

    fn get_executor(&self) -> &Self::WitnessExecutor {
        &self.executor
    }

    fn get_sp1_stdin(&self, witness: Self::WitnessData) -> Result<SP1Stdin> {
        let mut stdin = SP1Stdin::default();
        let buffer = to_bytes::<rkyv::rancor::Error>(&witness)?;
        stdin.write_slice(&buffer);
        Ok(stdin)
    }
}
