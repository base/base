//! Inherent chain specification accessors.

use alloy_eips::{calc_next_block_base_fee, eip7840::BlobParams};
use alloy_primitives::U256;
use reth_ethereum_forks::EthereumHardforks;
use reth_primitives_traits::BlockHeader;

use crate::{ChainSpec, DepositContract};

impl<H: BlockHeader> ChainSpec<H> {
    /// Get the [`BlobParams`] for the given timestamp
    pub fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        if let Some(blob_param) = self.blob_params.active_scheduled_params_at_timestamp(timestamp) {
            Some(*blob_param)
        } else if self.is_osaka_active_at_timestamp(timestamp) {
            Some(self.blob_params.osaka)
        } else if self.is_prague_active_at_timestamp(timestamp) {
            Some(self.blob_params.prague)
        } else if self.is_cancun_active_at_timestamp(timestamp) {
            Some(self.blob_params.cancun)
        } else {
            None
        }
    }

    /// Returns the deposit contract data for the chain, if it's present
    pub fn deposit_contract(&self) -> Option<&DepositContract> {
        self.deposit_contract.as_ref()
    }

    /// The delete limit for pruner, per run.
    pub fn prune_delete_limit(&self) -> usize {
        self.prune_delete_limit
    }

    /// Returns `true` if this chain contains Optimism configuration.
    pub fn is_optimism(&self) -> bool {
        false
    }

    /// Returns the final total difficulty if the Paris hardfork is known.
    pub fn final_paris_total_difficulty(&self) -> Option<U256> {
        self.get_final_paris_total_difficulty()
    }

    /// Returns the chain id number
    pub fn chain_id(&self) -> u64 {
        self.chain().id()
    }

    /// See [`calc_next_block_base_fee`].
    pub fn next_block_base_fee(&self, parent: &H, target_timestamp: u64) -> Option<u64> {
        Some(calc_next_block_base_fee(
            parent.gas_used(),
            parent.gas_limit(),
            parent.base_fee_per_gas()?,
            self.base_fee_params_at_timestamp(target_timestamp),
        ))
    }
}
