//! Shadow-sequencer account funding transaction construction.

use alloy_eips::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use base_common_consensus::{BaseTxEnvelope, TxDeposit};

/// Funding injected into the first private block of each shadow sequencing cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadowFunding {
    /// Account credited by the synthetic deposit.
    pub address: Address,
    /// Amount credited in wei.
    pub amount: U256,
}

impl ShadowFunding {
    /// Creates a shadow funding configuration.
    pub const fn new(address: Address, amount: U256) -> Self {
        Self { address, amount }
    }

    /// Encodes the synthetic deposit transaction for a cycle beginning at `parent_hash`.
    pub fn transaction(self, parent_hash: B256) -> Bytes {
        let source_hash = keccak256(
            [
                b"base-shadow-sequencer-funding-v1".as_slice(),
                parent_hash.as_slice(),
                self.address.as_slice(),
                &self.amount.to_be_bytes::<32>(),
            ]
            .concat(),
        );
        BaseTxEnvelope::from(TxDeposit {
            source_hash,
            from: self.address,
            to: TxKind::Call(self.address),
            mint: self.amount.to::<u128>(),
            value: U256::ZERO,
            gas_limit: 21_000,
            is_system_transaction: false,
            input: Bytes::new(),
        })
        .encoded_2718()
        .into()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, utils::parse_ether};
    use alloy_provider::Provider;
    use base_node_runner::test_utils::TestHarness;

    use super::ShadowFunding;

    #[tokio::test]
    async fn funding_transaction_mints_configured_balance_in_execution_layer() {
        let harness = TestHarness::new().await.unwrap();
        let address = Address::repeat_byte(0x42);
        let amount = parse_ether("12345").unwrap();
        let balance_before = harness.provider().get_balance(address).await.unwrap();
        let funding = ShadowFunding::new(address, amount);

        harness
            .build_block_from_transactions(vec![funding.transaction(B256::repeat_byte(0x11))])
            .await
            .unwrap();

        assert_eq!(harness.provider().get_balance(address).await.unwrap(), balance_before + amount);
    }
}
