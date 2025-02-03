//! Module containing a [Transaction] builder for the Isthmus network upgrade transactions.
//!
//! Isthmus network upgrade transactions are defined in the [OP Stack Specs][specs].
//!
//! [specs]: https://specs.optimism.io/protocol/isthmus/derivation.html#network-upgrade-automation-transactions
//! [Transaction]: alloy_consensus::Transaction

use crate::UpgradeDepositSource;
use alloc::{string::String, vec::Vec};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{address, hex, Address, Bytes, TxKind, B256, U256};

use crate::{Hardfork, TxDeposit};

/// The Isthmus network upgrade transactions.
#[derive(Debug, Default, Clone, Copy)]
pub struct Isthmus;

impl Isthmus {
    /// EIP-2935 From Address
    pub const EIP2935_FROM: Address = address!("3462413Af4609098e1E27A490f554f260213D685");

    /// Returns the source hash for the Isthmus Deposit Contract deployment.
    pub fn deposit_contract_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Isthmus: deposit contract deployment") }
            .source_hash()
    }

    /// Returns the EIP-2935 creation data.
    pub fn eip2935_creation_data() -> Bytes {
        hex::decode(include_str!("./bytecode/eip2935_isthmus.hex").replace("\n", ""))
            .expect("Expected hex byte string")
            .into()
    }

    /// Returns the list of [TxDeposit]s for the network upgrade.
    pub fn deposits() -> impl Iterator<Item = TxDeposit> {
        ([TxDeposit {
            source_hash: Self::deposit_contract_source(),
            from: Self::EIP2935_FROM,
            to: TxKind::Create,
            mint: 0.into(),
            value: U256::ZERO,
            gas_limit: 250_000,
            is_system_transaction: false,
            input: Self::eip2935_creation_data(),
        }])
        .into_iter()
    }
}

impl Hardfork for Isthmus {
    /// Constructs the network upgrade transactions.
    fn txs(&self) -> impl Iterator<Item = Bytes> + '_ {
        Self::deposits().map(|tx| {
            let mut encoded = Vec::new();
            tx.encode_2718(&mut encoded);
            Bytes::from(encoded)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn test_isthmus_txs_encoded() {
        let isthmus_upgrade_tx = Isthmus.txs().collect::<Vec<_>>();
        assert_eq!(isthmus_upgrade_tx.len(), 1);

        let expected_txs: Vec<Bytes> =
            vec![hex::decode(include_str!("./bytecode/isthmus_tx_1.hex").replace("\n", ""))
                .unwrap()
                .into()];
        for (i, expected) in expected_txs.iter().enumerate() {
            assert_eq!(isthmus_upgrade_tx[i], *expected);
        }
    }
}
