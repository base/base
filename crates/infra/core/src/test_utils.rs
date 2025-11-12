use crate::{AcceptedBundle, Bundle, MeterBundleResponse};
use alloy_consensus::SignableTransaction;
use alloy_primitives::{Address, B256, Bytes, TxHash, U256, b256, bytes};
use alloy_provider::network::TxSignerSync;
use alloy_provider::network::eip2718::Encodable2718;
use alloy_signer_local::PrivateKeySigner;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::OpTransactionRequest;

// https://basescan.org/tx/0x4f7ddfc911f5cf85dd15a413f4cbb2a0abe4f1ff275ed13581958c0bcf043c5e
pub const TXN_DATA: Bytes = bytes!(
    "0x02f88f8221058304b6b3018315fb3883124f80948ff2f0a8d017c79454aa28509a19ab9753c2dd1480a476d58e1a0182426068c9ea5b00000000000000000002f84f00000000083e4fda54950000c080a086fbc7bbee41f441fb0f32f7aa274d2188c460fe6ac95095fa6331fa08ec4ce7a01aee3bcc3c28f7ba4e0c24da9ae85e9e0166c73cabb42c25ff7b5ecd424f3105"
);

pub const TXN_HASH: TxHash =
    b256!("0x4f7ddfc911f5cf85dd15a413f4cbb2a0abe4f1ff275ed13581958c0bcf043c5e");

pub fn create_bundle_from_txn_data() -> AcceptedBundle {
    AcceptedBundle::new(
        Bundle {
            txs: vec![TXN_DATA.clone()],
            ..Default::default()
        }
        .try_into()
        .unwrap(),
        create_test_meter_bundle_response(),
    )
}

pub fn create_transaction(from: PrivateKeySigner, nonce: u64, to: Address) -> OpTxEnvelope {
    let mut txn = OpTransactionRequest::default()
        .value(U256::from(10_000))
        .gas_limit(21_000)
        .max_fee_per_gas(200)
        .max_priority_fee_per_gas(100)
        .from(from.address())
        .to(to)
        .nonce(nonce)
        .build_typed_tx()
        .unwrap();

    let sig = from.sign_transaction_sync(&mut txn).unwrap();
    OpTxEnvelope::Eip1559(txn.eip1559().cloned().unwrap().into_signed(sig).clone())
}

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
