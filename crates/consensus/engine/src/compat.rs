//! Compatibility helpers for converting between upstream `op_alloy` types
//! and local `base_alloy` types.

use alloy_consensus::{Block, Sealed};
use base_alloy_consensus::{OpTxEnvelope, TxDeposit};

/// Converts an upstream [`op_alloy_consensus::OpTxEnvelope`] into a
/// [`base_alloy_consensus::OpTxEnvelope`].
///
/// The non-deposit variants share the same underlying alloy-consensus signed
/// transaction types, so they are moved directly. Only the `Deposit` variant
/// requires field-by-field conversion of the inner [`TxDeposit`].
pub(crate) fn to_base_tx(tx: op_alloy_consensus::OpTxEnvelope) -> OpTxEnvelope {
    match tx {
        op_alloy_consensus::OpTxEnvelope::Legacy(s) => OpTxEnvelope::Legacy(s),
        op_alloy_consensus::OpTxEnvelope::Eip2930(s) => OpTxEnvelope::Eip2930(s),
        op_alloy_consensus::OpTxEnvelope::Eip1559(s) => OpTxEnvelope::Eip1559(s),
        op_alloy_consensus::OpTxEnvelope::Eip7702(s) => OpTxEnvelope::Eip7702(s),
        op_alloy_consensus::OpTxEnvelope::Deposit(sealed) => {
            let hash = sealed.seal();
            let d = sealed.into_inner();
            OpTxEnvelope::Deposit(Sealed::new_unchecked(
                TxDeposit {
                    source_hash: d.source_hash,
                    from: d.from,
                    to: d.to,
                    mint: d.mint,
                    value: d.value,
                    gas_limit: d.gas_limit,
                    is_system_transaction: d.is_system_transaction,
                    input: d.input,
                },
                hash,
            ))
        }
    }
}

/// Converts a [`base_alloy_rpc_types_engine::OpPayloadAttributes`] into an
/// upstream [`op_alloy_rpc_types_engine::OpPayloadAttributes`].
pub(crate) fn to_upstream_attrs(
    a: base_alloy_rpc_types_engine::OpPayloadAttributes,
) -> op_alloy_rpc_types_engine::OpPayloadAttributes {
    op_alloy_rpc_types_engine::OpPayloadAttributes {
        payload_attributes: a.payload_attributes,
        transactions: a.transactions,
        no_tx_pool: a.no_tx_pool,
        gas_limit: a.gas_limit,
        eip_1559_params: a.eip_1559_params,
        min_base_fee: a.min_base_fee,
    }
}

/// Converts an L2 RPC consensus block into a [`Block`] with base
/// [`OpTxEnvelope`] transactions.
pub(crate) fn rpc_block_to_base(
    block: Block<op_alloy_rpc_types::Transaction>,
) -> Block<OpTxEnvelope> {
    block.map_transactions(|tx| to_base_tx(tx.inner.inner.into_inner()))
}

/// Converts an upstream [`op_alloy_consensus::OpBlock`] into a base
/// [`Block<OpTxEnvelope>`].
pub(crate) fn op_block_to_base(block: op_alloy_consensus::OpBlock) -> Block<OpTxEnvelope> {
    block.map_transactions(to_base_tx)
}

/// Converts a [`base_alloy_rpc_types_engine::OpExecutionPayloadEnvelope`] into an
/// upstream [`op_alloy_rpc_types_engine::OpExecutionPayloadEnvelope`].
pub fn to_upstream_envelope(
    e: base_alloy_rpc_types_engine::OpExecutionPayloadEnvelope,
) -> op_alloy_rpc_types_engine::OpExecutionPayloadEnvelope {
    op_alloy_rpc_types_engine::OpExecutionPayloadEnvelope {
        parent_beacon_block_root: e.parent_beacon_block_root,
        execution_payload: to_upstream_payload(e.execution_payload),
    }
}

/// Converts an upstream [`op_alloy_rpc_types_engine::OpExecutionPayloadEnvelope`] into a
/// [`base_alloy_rpc_types_engine::OpExecutionPayloadEnvelope`].
pub fn to_base_envelope(
    e: op_alloy_rpc_types_engine::OpExecutionPayloadEnvelope,
) -> base_alloy_rpc_types_engine::OpExecutionPayloadEnvelope {
    base_alloy_rpc_types_engine::OpExecutionPayloadEnvelope {
        parent_beacon_block_root: e.parent_beacon_block_root,
        execution_payload: to_base_payload(e.execution_payload),
    }
}

/// Converts a [`base_alloy_rpc_types_engine::OpExecutionPayload`] into an
/// upstream [`op_alloy_rpc_types_engine::OpExecutionPayload`].
fn to_upstream_payload(
    p: base_alloy_rpc_types_engine::OpExecutionPayload,
) -> op_alloy_rpc_types_engine::OpExecutionPayload {
    match p {
        base_alloy_rpc_types_engine::OpExecutionPayload::V1(v) => {
            op_alloy_rpc_types_engine::OpExecutionPayload::V1(v)
        }
        base_alloy_rpc_types_engine::OpExecutionPayload::V2(v) => {
            op_alloy_rpc_types_engine::OpExecutionPayload::V2(v)
        }
        base_alloy_rpc_types_engine::OpExecutionPayload::V3(v) => {
            op_alloy_rpc_types_engine::OpExecutionPayload::V3(v)
        }
        base_alloy_rpc_types_engine::OpExecutionPayload::V4(v) => {
            op_alloy_rpc_types_engine::OpExecutionPayload::V4(
                op_alloy_rpc_types_engine::OpExecutionPayloadV4 {
                    payload_inner: v.payload_inner,
                    withdrawals_root: v.withdrawals_root,
                },
            )
        }
    }
}

/// Converts an upstream [`op_alloy_rpc_types_engine::OpExecutionPayload`] into a
/// [`base_alloy_rpc_types_engine::OpExecutionPayload`].
pub(crate) fn to_base_payload(
    p: op_alloy_rpc_types_engine::OpExecutionPayload,
) -> base_alloy_rpc_types_engine::OpExecutionPayload {
    match p {
        op_alloy_rpc_types_engine::OpExecutionPayload::V1(v) => {
            base_alloy_rpc_types_engine::OpExecutionPayload::V1(v)
        }
        op_alloy_rpc_types_engine::OpExecutionPayload::V2(v) => {
            base_alloy_rpc_types_engine::OpExecutionPayload::V2(v)
        }
        op_alloy_rpc_types_engine::OpExecutionPayload::V3(v) => {
            base_alloy_rpc_types_engine::OpExecutionPayload::V3(v)
        }
        op_alloy_rpc_types_engine::OpExecutionPayload::V4(v) => {
            base_alloy_rpc_types_engine::OpExecutionPayload::V4(
                base_alloy_rpc_types_engine::OpExecutionPayloadV4 {
                    payload_inner: v.payload_inner,
                    withdrawals_root: v.withdrawals_root,
                },
            )
        }
    }
}
