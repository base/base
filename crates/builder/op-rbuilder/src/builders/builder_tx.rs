use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives::Recovered;

use crate::tx_signer::Signer;

pub trait BuilderTx {
    fn estimated_builder_tx_gas(&self) -> u64;
    fn estimated_builder_tx_da_size(&self) -> Option<u64>;
    fn signed_builder_tx(&self) -> Result<Recovered<OpTransactionSigned>, secp256k1::Error>;
}

// Scaffolding for how to construct the end of block builder transaction
// This will be the regular end of block transaction without the TEE key
#[derive(Clone)]
pub(super) struct StandardBuilderTx {
    #[allow(dead_code)]
    pub signer: Option<Signer>,
}

impl BuilderTx for StandardBuilderTx {
    fn estimated_builder_tx_gas(&self) -> u64 {
        todo!()
    }

    fn estimated_builder_tx_da_size(&self) -> Option<u64> {
        todo!()
    }

    fn signed_builder_tx(&self) -> Result<Recovered<OpTransactionSigned>, secp256k1::Error> {
        todo!()
    }
}
