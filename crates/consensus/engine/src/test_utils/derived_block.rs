//! The block an EL holds for derived attributes, and the L1 info deposit it starts with.

use alloy_consensus::transaction::Recovered;
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, Bytes};
use alloy_rpc_types_eth::{Block as RpcBlock, BlockTransactions};
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use base_common_rpc_types::Transaction as BaseTransaction;
use base_protocol::{AttributesWithParent, L1BlockInfoBedrock};

/// The L1 info deposit an L2 block starts with, carrying `l1_info`.
pub fn l1_info_deposit_tx(l1_info: L1BlockInfoBedrock) -> BaseTxEnvelope {
    BaseTxEnvelope::from(TxDeposit { input: l1_info.encode_calldata(), ..Default::default() })
}

/// [`l1_info_deposit_tx`] encoded as derived attributes carry it.
pub fn encoded_l1_info_deposit_tx(l1_info: L1BlockInfoBedrock) -> Bytes {
    l1_info_deposit_tx(l1_info).encoded_2718().into()
}

/// `tx` as an RPC transaction of the block numbered `block_number`.
pub const fn rpc_transaction(tx: BaseTxEnvelope, block_number: u64) -> BaseTransaction {
    BaseTransaction {
        inner: alloy_rpc_types_eth::Transaction {
            inner: Recovered::new_unchecked(tx, Address::ZERO),
            block_hash: None,
            block_number: Some(block_number),
            block_timestamp: None,
            effective_gas_price: Some(0),
            transaction_index: Some(0),
        },
        block_timestamp_ms: None,
        deposit_nonce: None,
        deposit_receipt_version: None,
    }
}

/// The RPC block an EL holds for `attributes`, which
/// [`AttributesMatch`](crate::AttributesMatch) finds identical to them.
pub fn matching_rpc_block(attributes: &AttributesWithParent) -> RpcBlock<BaseTransaction> {
    let number = attributes.block_number();
    let payload_attributes = &attributes.attributes().payload_attributes;
    let mut block = RpcBlock::<BaseTransaction>::default();
    block.header.inner.number = number;
    block.header.inner.parent_hash = attributes.parent.block_info.hash;
    block.header.inner.timestamp = payload_attributes.timestamp;
    block.header.inner.mix_hash = payload_attributes.prev_randao;
    block.header.inner.gas_limit = attributes.attributes().gas_limit.unwrap_or_default();
    block.header.inner.parent_beacon_block_root = payload_attributes.parent_beacon_block_root;
    block.header.inner.beneficiary = payload_attributes.suggested_fee_recipient;
    block.header.hash = block.header.inner.hash_slow();
    block.transactions = BlockTransactions::Full(
        attributes
            .attributes()
            .transactions
            .iter()
            .flatten()
            .map(|tx| {
                let tx = BaseTxEnvelope::decode_2718(&mut tx.as_ref())
                    .expect("derived attributes carry encoded transactions");
                rpc_transaction(tx, number)
            })
            .collect(),
    );
    block
}
