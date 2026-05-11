//! Module containing a [`TxDeposit`] builder for the Base V1 network upgrade transactions.
//!
//! Base V1 network upgrade transactions are defined in the [Base Specs][specs].
//!
//! [specs]: https://specs.base.org/upgrades/base-v1/derivation#network-upgrade-automation-transactions

use alloy_primitives::{Address, Bytes};
use base_common_consensus::Deployers;

use crate::Upgrade;

/// The Base V1 network upgrade transactions.
#[derive(Debug, Default, Clone, Copy)]
pub struct BaseV1;

impl BaseV1 {
    upgrade_source_fn!(
        /// Returns the source hash for the deployment of the l1 block contract.
        deploy_l1_block_source,
        "Base V1: L1 Block Deployment"
    );

    upgrade_source_fn!(
        /// Returns the source hash for the l1 block proxy update.
        l1_block_proxy_update,
        "Base V1: L1 Block Proxy Update"
    );

    /// The Base V1 L1 Block Address
    pub fn l1_block_address() -> Address {
        Deployers::JOVIAN_L1_BLOCK.create(0)
    }
}

impl Upgrade for BaseV1 {
    /// Constructs the network upgrade transactions.
    fn txs(&self) -> impl Iterator<Item = Bytes> + '_ {
        core::iter::empty()
    }
}
