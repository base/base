//! Module containing a [`TxDeposit`] builder for the Fjord network upgrade transactions.

use alloc::vec::Vec;

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, hex};
use base_common_consensus::{Deployers, Predeploys, SystemAddresses, TxDeposit};

use crate::{Hardfork, UpgradeCalldata};

/// The Fjord network upgrade transactions.
#[derive(Debug, Default, Clone, Copy)]
pub struct Fjord;

impl Fjord {
    /// The Gas Price Oracle Address
    /// This is computed by using go-ethereum's `crypto.CreateAddress` function,
    /// with the Gas Price Oracle Deployer Address and nonce 0.
    pub const GAS_PRICE_ORACLE: Address = address!("b528d11cc114e026f138fe568744c6d45ce6da7a");

    /// Fjord Gas Price Oracle address.
    pub const FJORD_GAS_PRICE_ORACLE: Address =
        address!("a919894851548179a0750865e7974da599c0fac7");

    /// The Set Fjord Four Byte Method Signature.
    pub const SET_FJORD_METHOD_SIGNATURE: [u8; 4] = hex!("8e98b106");

    /// The Fjord Gas Price Oracle code hash.
    /// See: <https://specs.base.org/upgrades/fjord/derivation#gaspriceoracle-deployment>
    pub const GAS_PRICE_ORACLE_CODE_HASH: B256 = alloy_primitives::b256!(
        "0xa88fa50a2745b15e6794247614b5298483070661adacb8d32d716434ed24c6b2"
    );

    upgrade_source_fn!(
        /// Returns the source hash for the deployment of the Fjord Gas Price Oracle.
        deploy_fjord_gas_price_oracle_source,
        "Fjord: Gas Price Oracle Deployment"
    );

    upgrade_source_fn!(
        /// Returns the source hash for the update of the Fjord Gas Price Oracle.
        update_fjord_gas_price_oracle_source,
        "Fjord: Gas Price Oracle Proxy Update"
    );

    upgrade_source_fn!(
        /// Returns the source hash for setting the Fjord Gas Price Oracle.
        enable_fjord_source,
        "Fjord: Gas Price Oracle Set Fjord"
    );

    /// Returns the fjord gas price oracle deployment bytecode.
    pub fn gas_price_oracle_deployment_bytecode() -> alloy_primitives::Bytes {
        bytecode_from_hex!("./bytecode/gpo_fjord.hex")
    }

    /// Returns the list of [`TxDeposit`]s for the Fjord network upgrade.
    pub fn deposits() -> impl Iterator<Item = TxDeposit> {
        ([
            // Deploys the Fjord Gas Price Oracle contract.
            // See: <https://specs.base.org/upgrades/fjord/derivation#gaspriceoracle-deployment>
            TxDeposit {
                source_hash: Self::deploy_fjord_gas_price_oracle_source(),
                from: Deployers::FJORD_GAS_PRICE_ORACLE,
                to: TxKind::Create,
                mint: 0,
                value: U256::ZERO,
                gas_limit: 1_450_000,
                is_system_transaction: false,
                input: Self::gas_price_oracle_deployment_bytecode(),
            },
            // Updates the gas price Oracle proxy to point to the Fjord Gas Price Oracle.
            // See: <https://specs.base.org/upgrades/fjord/derivation#gaspriceoracle-proxy-update>
            TxDeposit {
                source_hash: Self::update_fjord_gas_price_oracle_source(),
                from: Address::ZERO,
                to: TxKind::Call(Predeploys::GAS_PRICE_ORACLE),
                mint: 0,
                value: U256::ZERO,
                gas_limit: 50_000,
                is_system_transaction: false,
                input: UpgradeCalldata::build(Self::FJORD_GAS_PRICE_ORACLE),
            },
            // Enables the Fjord Gas Price Oracle.
            // See: <https://specs.base.org/upgrades/fjord/derivation#gaspriceoracle-enable-fjord>
            TxDeposit {
                source_hash: Self::enable_fjord_source(),
                from: SystemAddresses::DEPOSITOR_ACCOUNT,
                to: TxKind::Call(Predeploys::GAS_PRICE_ORACLE),
                mint: 0,
                value: U256::ZERO,
                gas_limit: 90_000,
                is_system_transaction: false,
                input: Self::SET_FJORD_METHOD_SIGNATURE.into(),
            },
        ])
        .into_iter()
    }
}

impl Hardfork for Fjord {
    /// Constructs the Fjord network upgrade transactions.
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
    use alloc::vec;

    use rstest::rstest;

    use super::*;
    use crate::test_utils::check_deployment_code;

    #[rstest]
    #[case(Fjord::deploy_fjord_gas_price_oracle_source(), hex!("86122c533fdcb89b16d8713174625e44578a89751d96c098ec19ab40a51a8ea3"))]
    #[case(Fjord::update_fjord_gas_price_oracle_source(), hex!("1e6bb0c28bfab3dc9b36ffb0f721f00d6937f33577606325692db0965a7d58c6"))]
    #[case(Fjord::enable_fjord_source(), hex!("bac7bb0d5961cad209a345408b0280a0d4686b1b20665e1b0f9cdafd73b19b6b"))]
    fn test_fjord_source_hashes(#[case] actual: B256, #[case] expected: [u8; 32]) {
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_fjord_txs_encoded() {
        let fjord_upgrade_tx = Fjord.txs().collect::<Vec<_>>();
        assert_eq!(fjord_upgrade_tx.len(), 3);

        let expected_txs: Vec<Bytes> = vec![
            bytecode_from_hex!("./bytecode/fjord_tx_0.hex"),
            bytecode_from_hex!("./bytecode/fjord_tx_1.hex"),
            bytecode_from_hex!("./bytecode/fjord_tx_2.hex"),
        ];
        for (i, expected) in expected_txs.iter().enumerate() {
            assert_eq!(fjord_upgrade_tx[i], *expected);
        }
    }

    #[test]
    fn test_verify_fjord_gas_price_oracle_deployment_code_hash() {
        let txs = Fjord::deposits().collect::<Vec<_>>();

        check_deployment_code(
            txs[0].clone(),
            Fjord::FJORD_GAS_PRICE_ORACLE,
            Fjord::GAS_PRICE_ORACLE_CODE_HASH,
        );
    }
}
