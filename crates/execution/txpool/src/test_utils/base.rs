//! Base transactions using the shared pool cache for network fixtures.
//!
//! Real Base envelopes provide hashing, encoding, and signer recovery. The higher-level Base
//! pool depends on node construction, so it cannot be used by the networking test harness.

use std::sync::Arc;

use alloy_eips::{
    eip2718::Encodable2718, eip4844::env_settings::KzgSettings,
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{Address, TxHash, U256};
use base_common_consensus::{
    BasePooledTransaction, BaseTxEnvelope, BlobTransactionValidationError, Typed2718,
};
use reth_primitives_traits::Recovered;

use crate::{EthBlobTransactionSidecar, EthPoolTransaction, EthPooledTransaction, PoolTransaction};

/// Cached Base transaction for shared execution infrastructure tests.
pub type BaseTestTransaction = EthPooledTransaction<BaseTxEnvelope>;

impl PoolTransaction for BaseTestTransaction {
    type TryFromConsensusError = <BasePooledTransaction as TryFrom<BaseTxEnvelope>>::Error;
    type Consensus = BaseTxEnvelope;
    type Pooled = BasePooledTransaction;

    fn clone_into_consensus(&self) -> Recovered<Self::Consensus> {
        self.transaction.clone()
    }

    fn consensus_ref(&self) -> Recovered<&Self::Consensus> {
        self.transaction.as_recovered_ref()
    }

    fn into_consensus(self) -> Recovered<Self::Consensus> {
        self.transaction
    }

    fn from_pooled(tx: Recovered<Self::Pooled>) -> Self {
        let encoded_length = tx.encode_2718_len();
        Self::new(tx.convert(), encoded_length)
    }

    fn hash(&self) -> &TxHash {
        self.transaction.inner().hash()
    }

    fn sender(&self) -> Address {
        self.transaction.signer()
    }

    fn sender_ref(&self) -> &Address {
        self.transaction.signer_ref()
    }

    fn cost(&self) -> &U256 {
        &self.cost
    }

    fn encoded_length(&self) -> usize {
        self.encoded_length
    }
}

impl EthPoolTransaction for BaseTestTransaction {
    fn take_blob(&mut self) -> EthBlobTransactionSidecar {
        EthBlobTransactionSidecar::None
    }

    fn try_into_pooled_eip4844(
        self,
        _sidecar: Arc<BlobTransactionSidecarVariant>,
    ) -> Option<Recovered<Self::Pooled>> {
        None
    }

    fn try_from_eip4844(
        _tx: Recovered<Self::Consensus>,
        _sidecar: BlobTransactionSidecarVariant,
    ) -> Option<Self> {
        None
    }

    fn validate_blob(
        &self,
        _sidecar: &BlobTransactionSidecarVariant,
        _settings: &KzgSettings,
    ) -> Result<(), BlobTransactionValidationError> {
        Err(BlobTransactionValidationError::NotBlobTransaction(self.ty()))
    }
}
