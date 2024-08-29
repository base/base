//! Module containing a [Transaction] builder for the Fjord network upgrade transactions.
//!
//! [Transaction]: alloy_consensus::Transaction

use crate::{OpTxEnvelope, TxDeposit};
#[cfg(not(feature = "std"))]
use alloc::{string::String, vec, vec::Vec};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use spin::Lazy;

use crate::UpgradeDepositSource;

/// The L1 Info Depositer Address.
pub const L1_INFO_DEPOSITER_ADDRESS: Address = address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001");

/// Fjord Gas Price Oracle Deployer Address.
pub const GAS_PRICE_ORACLE_FJORD_DEPLOYER_ADDRESS: Address =
    address!("4210000000000000000000000000000000000002");

/// The Gas Price Oracle Address
/// This is computed by using go-ethereum's `crypto.CreateAddress` function,
/// with the Gas Price Oracle Deployer Address and nonce 0.
pub const GAS_PRICE_ORACLE_ADDRESS: Address = address!("b528d11cc114e026f138fe568744c6d45ce6da7a");

/// Fjord Gas Price Oracle source.
static DEPLOY_FJORD_GAS_PRICE_ORACLE_SOURCE: Lazy<UpgradeDepositSource> = Lazy::new(|| {
    UpgradeDepositSource { intent: String::from("Fjord: Gas Price Oracle Deployment") }
});

/// [UpgradeDepositSource] for the source code update to the Fjord Gas Price Oracle.
static UPDATE_FJORD_GAS_PRICE_ORACLE_SOURCE: Lazy<UpgradeDepositSource> = Lazy::new(|| {
    UpgradeDepositSource { intent: String::from("Fjord: Gas Price Oracle Proxy Update") }
});

/// Fjord Gas Price Oracle address.
pub const FJORD_GAS_PRICE_ORACLE_ADDRESS: Address =
    address!("a919894851548179a0750865e7974da599c0fac7");

/// [UpgradeDepositSource] for setting the Fjord Gas Price Oracle.
static ENABLE_FJORD_SOURCE: Lazy<UpgradeDepositSource> = Lazy::new(|| UpgradeDepositSource {
    intent: String::from("Fjord: Gas Price Oracle Set Fjord"),
});

/// Input data for setting the Fjord Gas Price Oracle.
pub const ENABLE_FJORD_FUNC_SIGNATURE: &str = "setFjord()";

/// The Set Fjord Four Byte Method Signature.
pub const SET_FJORD_METHOD_SIGNATURE: &[u8] = &[0x8e, 0x98, 0xb1, 0x06];

impl super::Hardforks {
    /// Returns the fjord gas price oracle deployment bytecode.
    pub(crate) fn gas_price_oracle_deployment_bytecode() -> alloy_primitives::Bytes {
        include_bytes!("./gas_price_oracle_bytecode.hex").into()
    }

    /// Constructs the Fjord network upgrade transactions.
    pub fn fjord_txs() -> Vec<Bytes> {
        let mut txs = vec![];

        let mut buffer = Vec::new();
        OpTxEnvelope::Deposit(TxDeposit {
            source_hash: DEPLOY_FJORD_GAS_PRICE_ORACLE_SOURCE.source_hash(),
            from: GAS_PRICE_ORACLE_FJORD_DEPLOYER_ADDRESS,
            to: TxKind::Create,
            mint: 0.into(),
            value: U256::ZERO,
            gas_limit: 1_450_000,
            is_system_transaction: false,
            input: Self::gas_price_oracle_deployment_bytecode(),
        })
        .encode_2718(&mut buffer);
        txs.push(Bytes::from(buffer));

        // Update the gas price oracle proxy.
        buffer = Vec::new();
        OpTxEnvelope::Deposit(TxDeposit {
            source_hash: UPDATE_FJORD_GAS_PRICE_ORACLE_SOURCE.source_hash(),
            from: Address::ZERO,
            to: TxKind::Call(GAS_PRICE_ORACLE_ADDRESS),
            mint: 0.into(),
            value: U256::ZERO,
            gas_limit: 50_000,
            is_system_transaction: false,
            input: Self::upgrade_to_calldata(FJORD_GAS_PRICE_ORACLE_ADDRESS),
        })
        .encode_2718(&mut buffer);
        txs.push(Bytes::from(buffer));

        // Enable Fjord
        buffer = Vec::new();
        OpTxEnvelope::Deposit(TxDeposit {
            source_hash: ENABLE_FJORD_SOURCE.source_hash(),
            from: L1_INFO_DEPOSITER_ADDRESS,
            to: TxKind::Call(GAS_PRICE_ORACLE_ADDRESS),
            mint: 0.into(),
            value: U256::ZERO,
            gas_limit: 90_000,
            is_system_transaction: false,
            input: SET_FJORD_METHOD_SIGNATURE.into(),
        })
        .encode_2718(&mut buffer);
        txs.push(Bytes::from(buffer));

        txs
    }
}
