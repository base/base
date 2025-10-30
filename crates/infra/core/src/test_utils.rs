use crate::{Bundle, BundleWithMetadata, MeterBundleResponse};
use alloy_consensus::SignableTransaction;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::network::TxSignerSync;
use alloy_provider::network::eip2718::Encodable2718;
use alloy_signer_local::PrivateKeySigner;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::OpTransactionRequest;

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
) -> BundleWithMetadata {
    let txs = txns.iter().map(|t| t.encoded_2718().into()).collect();

    let bundle = Bundle {
        txs,
        block_number: block_number.unwrap_or(0),
        min_timestamp,
        max_timestamp,
        ..Default::default()
    };
    let meter_bundle_response = create_test_meter_bundle_response();

    BundleWithMetadata::load(bundle, meter_bundle_response).unwrap()
}

pub fn create_test_meter_bundle_response() -> MeterBundleResponse {
    MeterBundleResponse {
        bundle_gas_price: "0".to_string(),
        bundle_hash: B256::default(),
        coinbase_diff: "0".to_string(),
        eth_sent_to_coinbase: "0".to_string(),
        gas_fees: "0".to_string(),
        results: vec![],
        state_block_number: 0,
        state_flashblock_index: None,
        total_gas_used: 0,
        total_execution_time_us: 0,
        state_root_time_us: 0,
    }
}
