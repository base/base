//! Module containing a [Transaction] builder for the Ecotone network upgrade transactions.
//!
//! [Transaction]: alloy_consensus::Transaction

use alloc::{string::String, vec, vec::Vec};
use alloy_consensus::Sealable;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{address, hex, Address, Bytes, TxKind, B256, U256};

use crate::{OpTxEnvelope, TxDeposit, UpgradeDepositSource, GAS_PRICE_ORACLE};

/// L1 Block Deployer Address
pub const L1_BLOCK_DEPLOYER: Address = address!("4210000000000000000000000000000000000000");

/// The Gas Price Oracle Deployer Address
pub const GAS_PRICE_ORACLE_DEPLOYER: Address = address!("4210000000000000000000000000000000000001");

/// The new L1 Block Address
/// This is computed by using go-ethereum's `crypto.CreateAddress` function,
/// with the L1 Block Deployer Address and nonce 0.
pub const NEW_L1_BLOCK: Address = address!("07dbe8500fc591d1852b76fee44d5a05e13097ff");

/// EIP-4788 From Address
pub const EIP4788_FROM: Address = address!("0B799C86a49DEeb90402691F1041aa3AF2d3C875");

impl super::Hardforks {
    /// The Enable Ecotone Input Method 4Byte Signature
    pub const ENABLE_ECOTONE_INPUT: [u8; 4] = hex!("22b908b3");

    /// Returns the source hash for the deployment of the l1 block contract.
    pub fn deploy_l1_block_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Ecotone: L1 Block Deployment") }.source_hash()
    }

    /// Returns the source hash for the deployment of the gas price oracle contract.
    pub fn deploy_gas_price_oracle_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Ecotone: Gas Price Oracle Deployment") }
            .source_hash()
    }

    /// Returns the source hash for the update of the l1 block proxy.
    pub fn update_l1_block_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Ecotone: L1 Block Proxy Update") }
            .source_hash()
    }

    /// Returns the source hash for the update of the gas price oracle proxy.
    pub fn update_gas_price_oracle_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Ecotone: Gas Price Oracle Proxy Update") }
            .source_hash()
    }

    /// Returns the source hash for the Ecotone Beacon Block Roots Contract deployment.
    pub fn beacon_roots_source() -> B256 {
        UpgradeDepositSource {
            intent: String::from("Ecotone: beacon block roots contract deployment"),
        }
        .source_hash()
    }

    /// Returns the source hash for the Ecotone Gas Price Oracle activation.
    pub fn enable_ecotone_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Ecotone: Gas Price Oracle Set Ecotone") }
            .source_hash()
    }

    /// Returns the EIP-4788 creation data.
    pub fn eip4788_creation_data() -> Bytes {
        include_bytes!("./bytecode/eip4788_ecotone.hex").into()
    }

    /// Returns the raw bytecode for the L1 Block deployment.
    pub fn l1_block_deployment_bytecode() -> Bytes {
        include_bytes!("./bytecode/l1_block_ecotone.hex").into()
    }

    /// Returns the gas price oracle deployment bytecode.
    pub fn ecotone_gas_price_oracle_deployment_bytecode() -> Bytes {
        include_bytes!("./bytecode/gpo_ecotone.hex").into()
    }

    /// Constructs the Ecotone network upgrade transactions.
    pub fn ecotone_txs() -> Vec<Bytes> {
        let mut txs = vec![];

        // Deploy the L1 Block Contract
        let mut buffer = Vec::new();
        OpTxEnvelope::Deposit(
            TxDeposit {
                source_hash: Self::deploy_l1_block_source(),
                from: L1_BLOCK_DEPLOYER,
                to: TxKind::Create,
                mint: 0.into(),
                value: U256::ZERO,
                gas_limit: 375_000,
                is_system_transaction: false,
                input: Self::l1_block_deployment_bytecode(),
            }
            .seal_slow(),
        )
        .encode_2718(&mut buffer);
        txs.push(Bytes::from(buffer));

        // Deploy the Gas Price Oracle
        buffer = Vec::new();
        OpTxEnvelope::Deposit(
            TxDeposit {
                source_hash: Self::deploy_gas_price_oracle_source(),
                from: GAS_PRICE_ORACLE_DEPLOYER,
                to: TxKind::Create,
                mint: 0.into(),
                value: U256::ZERO,
                gas_limit: 1_000_000,
                is_system_transaction: false,
                input: Self::ecotone_gas_price_oracle_deployment_bytecode(),
            }
            .seal_slow(),
        )
        .encode_2718(&mut buffer);
        txs.push(Bytes::from(buffer));

        // Update the l1 block proxy
        buffer = Vec::new();
        OpTxEnvelope::Deposit(
            TxDeposit {
                source_hash: Self::update_l1_block_source(),
                from: Address::default(),
                to: TxKind::Call(L1_BLOCK_DEPLOYER),
                mint: 0.into(),
                value: U256::ZERO,
                gas_limit: 50_000,
                is_system_transaction: false,
                input: Self::upgrade_to_calldata(NEW_L1_BLOCK),
            }
            .seal_slow(),
        )
        .encode_2718(&mut buffer);
        txs.push(Bytes::from(buffer));

        // Update gas price oracle proxy
        buffer = Vec::new();
        OpTxEnvelope::Deposit(
            TxDeposit {
                source_hash: Self::update_gas_price_oracle_source(),
                from: Address::default(),
                to: TxKind::Call(GAS_PRICE_ORACLE_DEPLOYER),
                mint: 0.into(),
                value: U256::ZERO,
                gas_limit: 50_000,
                is_system_transaction: false,
                input: Self::upgrade_to_calldata(GAS_PRICE_ORACLE),
            }
            .seal_slow(),
        )
        .encode_2718(&mut buffer);
        txs.push(Bytes::from(buffer));

        // Enable ecotone
        buffer = Vec::new();
        OpTxEnvelope::Deposit(
            TxDeposit {
                source_hash: Self::enable_ecotone_source(),
                from: L1_BLOCK_DEPLOYER,
                to: TxKind::Call(GAS_PRICE_ORACLE),
                mint: 0.into(),
                value: U256::ZERO,
                gas_limit: 80_000,
                is_system_transaction: false,
                input: Self::ENABLE_ECOTONE_INPUT.into(),
            }
            .seal_slow(),
        )
        .encode_2718(&mut buffer);
        txs.push(Bytes::from(buffer));

        // Deploy EIP-4788
        buffer = Vec::new();
        OpTxEnvelope::Deposit(
            TxDeposit {
                source_hash: Self::beacon_roots_source(),
                from: EIP4788_FROM,
                to: TxKind::Create,
                mint: 0.into(),
                value: U256::ZERO,
                gas_limit: 250_000,
                is_system_transaction: false,
                input: Self::eip4788_creation_data(),
            }
            .seal_slow(),
        )
        .encode_2718(&mut buffer);
        txs.push(Bytes::from(buffer));

        txs
    }
}
