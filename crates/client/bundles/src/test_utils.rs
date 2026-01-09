//! Test utilities for bundle types.

use alloy_consensus::SignableTransaction;
use alloy_primitives::{Address, B256, Bytes, TxHash, U256, b256, bytes};
use alloy_provider::network::{TxSignerSync, eip2718::Encodable2718};
use alloy_signer_local::PrivateKeySigner;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::OpTransactionRequest;

use crate::{AcceptedBundle, Bundle, MeterBundleResponse};

/// Sample transaction data from basescan.
///
/// <https://basescan.org/tx/0x4f7ddfc911f5cf85dd15a413f4cbb2a0abe4f1ff275ed13581958c0bcf043c5e>
pub const TXN_DATA: Bytes = bytes!(
    "0x02f88f8221058304b6b3018315fb3883124f80948ff2f0a8d017c79454aa28509a19ab9753c2dd1480a476d58e1a0182426068c9ea5b00000000000000000002f84f00000000083e4fda54950000c080a086fbc7bbee41f441fb0f32f7aa274d2188c460fe6ac95095fa6331fa08ec4ce7a01aee3bcc3c28f7ba4e0c24da9ae85e9e0166c73cabb42c25ff7b5ecd424f3105"
);

/// Hash of the sample transaction.
pub const TXN_HASH: TxHash =
    b256!("0x4f7ddfc911f5cf85dd15a413f4cbb2a0abe4f1ff275ed13581958c0bcf043c5e");

/// Creates a test bundle from the sample transaction data.
pub fn create_bundle_from_txn_data() -> AcceptedBundle {
    AcceptedBundle::new(
        Bundle { txs: vec![TXN_DATA], ..Default::default() }.try_into().unwrap(),
        create_test_meter_bundle_response(),
    )
}

/// Creates a signed transaction with the given parameters.
pub fn create_transaction(
    from: PrivateKeySigner,
    nonce: u64,
    to: Address,
    value: U256,
) -> OpTxEnvelope {
    let mut txn = OpTransactionRequest::default()
        .value(value)
        .gas_limit(21_000)
        .max_fee_per_gas(200)
        .max_priority_fee_per_gas(100)
        .from(from.address())
        .to(to)
        .nonce(nonce)
        .build_typed_tx()
        .unwrap();

    let sig = from.sign_transaction_sync(&mut txn).unwrap();
    OpTxEnvelope::Eip1559(txn.eip1559().cloned().unwrap().into_signed(sig))
}

/// Creates a test bundle with the given transactions and parameters.
pub fn create_test_bundle(
    txns: Vec<OpTxEnvelope>,
    block_number: Option<u64>,
    min_timestamp: Option<u64>,
    max_timestamp: Option<u64>,
) -> AcceptedBundle {
    let txs = txns.iter().map(|t| t.encoded_2718().into()).collect();

    let bundle = Bundle {
        txs,
        block_number: block_number.unwrap_or(0),
        min_timestamp,
        max_timestamp,
        ..Default::default()
    };
    let meter_bundle_response = create_test_meter_bundle_response();

    AcceptedBundle::new(bundle.try_into().unwrap(), meter_bundle_response)
}

/// Creates a default meter bundle response for testing.
pub fn create_test_meter_bundle_response() -> MeterBundleResponse {
    MeterBundleResponse {
        bundle_gas_price: U256::from(0),
        bundle_hash: B256::default(),
        coinbase_diff: U256::from(0),
        eth_sent_to_coinbase: U256::from(0),
        gas_fees: U256::from(0),
        results: vec![],
        state_block_number: 0,
        state_flashblock_index: None,
        total_gas_used: 0,
        total_execution_time_us: 0,
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Transaction;

    use super::*;
    use crate::traits::BundleExtensions;

    #[test]
    fn test_create_bundle_from_txn_data() {
        let bundle = create_bundle_from_txn_data();
        assert_eq!(bundle.txs.len(), 1);
        assert_eq!(bundle.txn_hashes()[0], TXN_HASH);
    }

    #[test]
    fn test_create_transaction() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice.clone(), 5, bob.address(), U256::from(1000));

        assert_eq!(tx.gas_limit(), 21_000);
    }

    #[test]
    fn test_create_test_bundle() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx1 = create_transaction(alice.clone(), 1, bob.address(), U256::from(100));
        let tx2 = create_transaction(alice.clone(), 2, bob.address(), U256::from(200));

        let bundle = create_test_bundle(vec![tx1, tx2], Some(100), Some(1000), Some(2000));

        assert_eq!(bundle.txs.len(), 2);
        assert_eq!(bundle.block_number, 100);
        assert_eq!(bundle.min_timestamp, Some(1000));
        assert_eq!(bundle.max_timestamp, Some(2000));
    }

    #[test]
    fn test_create_test_meter_bundle_response() {
        let response = create_test_meter_bundle_response();

        assert_eq!(response.bundle_gas_price, U256::ZERO);
        assert_eq!(response.coinbase_diff, U256::ZERO);
        assert!(response.results.is_empty());
        assert_eq!(response.state_block_number, 0);
        assert!(response.state_flashblock_index.is_none());
    }
}
