use crate::generator::{BlockCell, PayloadBuilder};
use alloy_consensus::Header;
use reth_basic_payload_builder::PayloadBuilder as RethPayloadBuilder; // Used to import the trait
use reth_basic_payload_builder::*;
use reth_chainspec::ChainSpecProvider;
use reth_evm::ConfigureEvm;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_payload_builder::payload::{OpBuiltPayload, OpPayloadBuilderAttributes};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_provider::StateProviderFactory;
use reth_transaction_pool::TransactionPool;

#[derive(Clone)]
pub struct VanillaOpPayloadBuilder<EvmConfig> {
    inner: reth_optimism_payload_builder::OpPayloadBuilder<EvmConfig>,
}

impl<EvmConfig> VanillaOpPayloadBuilder<EvmConfig> {
    /// `OpPayloadBuilder` constructor.
    pub const fn new(evm_config: EvmConfig) -> Self {
        Self {
            inner: reth_optimism_payload_builder::OpPayloadBuilder::new(evm_config),
        }
    }
}

impl<EvmConfig, Pool, Client> PayloadBuilder<Pool, Client> for VanillaOpPayloadBuilder<EvmConfig>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec>,
    Pool: TransactionPool,
    EvmConfig: ConfigureEvm<Header = Header>,
{
    type Attributes = OpPayloadBuilderAttributes;
    type BuiltPayload = OpBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Pool, Client, Self::Attributes, Self::BuiltPayload>,
        best_payload: BlockCell<Self::BuiltPayload>,
    ) -> Result<(), PayloadBuilderError> {
        match self.inner.try_build(args)? {
            BuildOutcome::Better { payload, .. } => {
                best_payload.set(payload);
                Ok(())
            }
            BuildOutcome::Freeze(payload) => {
                best_payload.set(payload);
                Ok(())
            }
            BuildOutcome::Cancelled => {
                tracing::warn!("Payload build cancelled");
                Err(PayloadBuilderError::MissingPayload)
            }
            _ => {
                tracing::warn!("No better payload found");
                Err(PayloadBuilderError::MissingPayload)
            }
        }
    }
}
