//! Module containing a [`TxDeposit`] builder for the Jovian network upgrade transactions.
//!
//! Jovian network upgrade transactions are defined in the [Base Specs][specs].
//!
//! [specs]: https://specs.base.org/upgrades/jovian/derivation#network-upgrade-automation-transactions

use alloc::vec::Vec;

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256, hex, keccak256};
use base_common_consensus::{Deployers, Predeploys, SystemAddresses, TxDeposit};

use crate::{Hardfork, UpgradeCalldata};

/// The Jovian network upgrade transactions.
#[derive(Debug, Default, Clone, Copy)]
pub struct Jovian;

impl Jovian {
    upgrade_source_fn!(
        /// Returns the source hash for the deployment of the l1 block contract.
        deploy_l1_block_source,
        "Jovian: L1 Block Deployment"
    );

    upgrade_source_fn!(
        /// Returns the source hash for the l1 block proxy update.
        l1_block_proxy_update,
        "Jovian: L1 Block Proxy Update"
    );

    upgrade_source_fn!(
        /// Returns the source hash for the deployment of the gas price oracle contract.
        gas_price_oracle,
        "Jovian: Gas Price Oracle Deployment"
    );

    upgrade_source_fn!(
        /// Returns the source hash for the gas price oracle proxy update.
        gas_price_oracle_proxy_update,
        "Jovian: Gas Price Oracle Proxy Update"
    );

    /// The Jovian L1 Block Address
    /// This is computed by using `Address::create` function,
    /// with the L1 Block Deployer Address and nonce 0.
    pub fn l1_block_address() -> Address {
        Deployers::JOVIAN_L1_BLOCK.create(0)
    }

    /// The Jovian Gas Price Oracle Address
    /// This is computed by using `Address::create` function,
    /// with the Gas Price Oracle Deployer Address and nonce 0.
    pub fn gas_price_oracle_address() -> Address {
        Deployers::JOVIAN_GAS_PRICE_ORACLE.create(0)
    }

    upgrade_source_fn!(
        /// Returns the source hash to enable the gas price oracle for Jovian.
        gas_price_oracle_enable_jovian,
        "Jovian: Gas Price Oracle Set Jovian"
    );

    /// Returns the raw bytecode for the L1 Block deployment.
    pub fn l1_block_deployment_bytecode() -> Bytes {
        bytecode_from_hex!("./bytecode/jovian-l1-block-deployment.hex")
    }

    /// Returns the gas price oracle deployment bytecode.
    pub fn gas_price_oracle_deployment_bytecode() -> Bytes {
        bytecode_from_hex!("./bytecode/jovian-gas-price-oracle-deployment.hex")
    }

    /// Returns the bytecode to enable the gas price oracle for Jovian.
    pub fn gas_price_oracle_enable_jovian_bytecode() -> Bytes {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&keccak256("setJovian()")[..4]);
        bytes.into()
    }

    /// Returns the list of [`TxDeposit`]s for the network upgrade.
    pub fn deposits() -> impl Iterator<Item = TxDeposit> {
        ([
            TxDeposit {
                source_hash: Self::deploy_l1_block_source(),
                from: Deployers::JOVIAN_L1_BLOCK,
                to: TxKind::Create,
                mint: 0,
                value: U256::ZERO,
                gas_limit: 447_315,
                is_system_transaction: false,
                input: Self::l1_block_deployment_bytecode(),
            },
            TxDeposit {
                source_hash: Self::l1_block_proxy_update(),
                from: Address::ZERO,
                to: TxKind::Call(Predeploys::L1_BLOCK_INFO),
                mint: 0,
                value: U256::ZERO,
                gas_limit: 50_000,
                is_system_transaction: false,
                input: UpgradeCalldata::build(Self::l1_block_address()),
            },
            TxDeposit {
                source_hash: Self::gas_price_oracle(),
                from: Deployers::JOVIAN_GAS_PRICE_ORACLE,
                to: TxKind::Create,
                mint: 0,
                value: U256::ZERO,
                gas_limit: 1_750_714,
                is_system_transaction: false,
                input: Self::gas_price_oracle_deployment_bytecode(),
            },
            TxDeposit {
                source_hash: Self::gas_price_oracle_proxy_update(),
                from: Address::ZERO,
                to: TxKind::Call(Predeploys::GAS_PRICE_ORACLE),
                mint: 0,
                value: U256::ZERO,
                gas_limit: 50_000,
                is_system_transaction: false,
                input: UpgradeCalldata::build(Self::gas_price_oracle_address()),
            },
            TxDeposit {
                source_hash: Self::gas_price_oracle_enable_jovian(),
                from: SystemAddresses::DEPOSITOR_ACCOUNT,
                to: TxKind::Call(Predeploys::GAS_PRICE_ORACLE),
                mint: 0,
                value: U256::ZERO,
                gas_limit: 90_000,
                is_system_transaction: false,
                input: Self::gas_price_oracle_enable_jovian_bytecode(),
            },
        ])
        .into_iter()
    }
}

impl Hardfork for Jovian {
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
    use alloy_primitives::{B256, b256};
    use rstest::rstest;

    use super::*;
    use crate::test_utils::check_deployment_code;

    #[rstest]
    #[case(Jovian::deploy_l1_block_source(), b256!("bb1a656f65401240fac3db12e7a79ebb954b11e62f7626eb11691539b798d3bf"))]
    #[case(Jovian::l1_block_proxy_update(), b256!("f3275f829340521028f9ad5bce4ecb1c64a45d448794effa2a77674627338e76"))]
    #[case(Jovian::gas_price_oracle(), b256!("239b7021a6c2cf3a918481242bbb5a9499057f24501539467536c691bb133962"))]
    #[case(Jovian::gas_price_oracle_proxy_update(), b256!("a70c60aa53b8c1c0d52b39b1e901e7d7c09f7819595cb24048a6bb1983b401ff"))]
    #[case(Jovian::gas_price_oracle_enable_jovian(), b256!("e836db6a959371756f8941be3e962d000f7e12a32e49e2c9ca42ba177a92716c"))]
    fn test_jovian_source_hashes(#[case] actual: B256, #[case] expected: B256) {
        assert_eq!(actual, expected);
    }

    #[rstest]
    #[case(Jovian::gas_price_oracle_address(), hex!("0x3659cfe60000000000000000000000004f1db3c6abd250ba86e0928471a8f7db3afd88f1"))]
    #[case(Jovian::l1_block_address(), hex!("0x3659cfe60000000000000000000000003ba4007f5c922fbb33c454b41ea7a1f11e83df2c"))]
    fn test_upgrade_calldata(#[case] addr: Address, #[case] expected: [u8; 36]) {
        assert_eq!(**UpgradeCalldata::build(addr), expected);
    }

    #[rstest]
    #[case(0, Jovian::l1_block_address(), hex!("5f885ca815d2cf27a203123e50b8ae204fdca910b6995d90b2d7700cbb9240d1").into())]
    #[case(2, Jovian::gas_price_oracle_address(), hex!("e9fc7c96c4db0d6078e3d359d7e8c982c350a513cb2c31121adf5e1e8a446614").into())]
    fn test_jovian_deployment_code_hashes(
        #[case] tx_idx: usize,
        #[case] addr: Address,
        #[case] code_hash: B256,
    ) {
        let txs = Jovian::deposits().collect::<Vec<_>>();
        check_deployment_code(txs[tx_idx].clone(), addr, code_hash);
    }

    #[test]
    fn test_verify_set_jovian() {
        let hash = &keccak256("setJovian()")[..4];
        assert_eq!(hash, hex!("0xb3d72079"))
    }
}
