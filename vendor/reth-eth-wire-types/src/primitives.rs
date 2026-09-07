//! Abstraction over primitive types in network messages.

use core::fmt::Debug;

use alloy_consensus::{RlpDecodableReceipt, RlpEncodableReceipt, TxReceipt};
use alloy_rlp::{Decodable, Encodable};
use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use reth_primitives_traits::{Block, BlockBody, BlockHeader, SignedTransaction};

use crate::NewBlockPayload;

/// Abstraction over primitive types which might appear in network messages.
///
/// Defines the representations used by the Ethereum Wire Protocol (devp2p).
/// Broadcast transactions and pooled transactions remain distinct because pooled
/// transactions can carry sidecars that are absent from the consensus block format.
///
/// See [`crate::EthMessage`] for more context.
pub trait NetworkPrimitives: Send + Sync + Unpin + Clone + Debug + 'static {
    /// The block header type.
    type BlockHeader: BlockHeader + 'static;

    /// The block body type.
    type BlockBody: BlockBody + 'static;

    /// Full block type.
    type Block: Block<Header = Self::BlockHeader, Body = Self::BlockBody>
        + Encodable
        + Decodable
        + 'static;

    /// The transaction type which peers announce in `Transactions` messages.
    ///
    /// This is different from `PooledTransactions` to account for the Ethereum case where
    /// EIP-4844 blob transactions are not announced over the network and can only be
    /// explicitly requested from peers. This is because blob transactions can be quite
    /// large and broadcasting them to all peers would cause
    /// significant bandwidth usage.
    type BroadcastedTransaction: SignedTransaction + 'static;

    /// The transaction type which peers return in `PooledTransactions` messages.
    ///
    /// For EIP-4844 blob transactions, this includes the full blob sidecar with
    /// KZG commitments and proofs that are needed for validation but are not
    /// included in the consensus block format.
    type PooledTransaction: SignedTransaction + TryFrom<Self::BroadcastedTransaction> + 'static;

    /// The transaction type which peers return in `GetReceipts` messages.
    type Receipt: TxReceipt
        + RlpEncodableReceipt
        + RlpDecodableReceipt
        + Encodable
        + Decodable
        + Unpin
        + 'static;

    /// The payload type for the `NewBlock` message.
    type NewBlockPayload: NewBlockPayload<Block = Self::Block>;
}

/// Base network primitives with configurable pooled transaction and new-block wire formats.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BasicNetworkPrimitives<Pooled, NewBlock = crate::NewBlock<BaseBlock>>(
    core::marker::PhantomData<(Pooled, NewBlock)>,
);

impl<Pooled, NewBlock> NetworkPrimitives for BasicNetworkPrimitives<Pooled, NewBlock>
where
    Pooled: SignedTransaction + TryFrom<BaseTxEnvelope> + 'static,
    NewBlock: NewBlockPayload<Block = BaseBlock>,
{
    type BlockHeader = alloy_consensus::Header;
    type BlockBody = alloy_consensus::BlockBody<BaseTxEnvelope>;
    type Block = BaseBlock;
    type BroadcastedTransaction = BaseTxEnvelope;
    type PooledTransaction = Pooled;
    type Receipt = BaseReceipt;
    type NewBlockPayload = NewBlock;
}

/// Network primitive types used by Ethereum networks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EthNetworkPrimitives;

impl NetworkPrimitives for EthNetworkPrimitives {
    type BlockHeader = alloy_consensus::Header;
    type BlockBody = reth_ethereum_primitives::BlockBody;
    type Block = reth_ethereum_primitives::Block;
    type BroadcastedTransaction = reth_ethereum_primitives::TransactionSigned;
    type PooledTransaction = reth_ethereum_primitives::PooledTransactionVariant;
    type Receipt = reth_ethereum_primitives::Receipt;
    type NewBlockPayload = crate::NewBlock<Self::Block>;
}
