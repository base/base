//! Wire fixtures combining Base blocks with Ethereum transaction gossip coverage.

use base_common_consensus::{BaseBlock, BaseBlockBody, BaseReceipt};
use reth_eth_wire_types::{NetworkPrimitives, NewBlock};
use reth_ethereum_primitives::{PooledTransactionVariant, TransactionSigned};

/// Uses Base providers while retaining blob-sidecar coverage for generic wire handling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TestNetworkPrimitives;

impl NetworkPrimitives for TestNetworkPrimitives {
    type BlockHeader = alloy_consensus::Header;
    type BlockBody = BaseBlockBody;
    type Block = BaseBlock;
    type BroadcastedTransaction = TransactionSigned;
    type PooledTransaction = PooledTransactionVariant;
    type Receipt = BaseReceipt;
    type NewBlockPayload = NewBlock<BaseBlock>;
}
