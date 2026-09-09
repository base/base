//! Types and traits for execution payload data structures.

use alloc::vec::Vec;
use core::fmt::Debug;

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent, eip4895::Withdrawal};
use alloy_primitives::{B256, Bytes};
use serde::{Serialize, de::DeserializeOwned};

/// Represents the core data structure of an execution payload.
///
/// Contains all necessary information to execute and validate a block, including
/// headers, transactions, and consensus fields. Provides a unified interface
/// regardless of protocol version.
pub trait ExecutionPayload:
    Serialize + DeserializeOwned + Debug + Clone + Send + Sync + 'static
{
    /// Returns the hash of this block's parent.
    fn parent_hash(&self) -> B256;

    /// Returns this block's hash.
    fn block_hash(&self) -> B256;

    /// Returns this block's number (height).
    fn block_number(&self) -> u64;

    /// Returns this block's number hash.
    fn num_hash(&self) -> BlockNumHash {
        BlockNumHash::new(self.block_number(), self.block_hash())
    }

    /// Returns a [`BlockWithParent`] for this block.
    fn block_with_parent(&self) -> BlockWithParent {
        BlockWithParent::new(self.parent_hash(), self.num_hash())
    }

    /// Returns the withdrawals included in this payload.
    ///
    /// Returns `None` for pre-Shanghai blocks.
    fn withdrawals(&self) -> Option<&Vec<Withdrawal>>;

    /// Returns the access list included in this payload.
    ///
    /// Returns `None` for pre-Amsterdam blocks.
    fn block_access_list(&self) -> Option<&Bytes>;

    /// Returns the beacon block root associated with this payload.
    ///
    /// Returns `None` for pre-merge payloads.
    fn parent_beacon_block_root(&self) -> Option<B256>;

    /// Returns this block's timestamp (seconds since Unix epoch).
    fn timestamp(&self) -> u64;

    /// Returns the total gas consumed by all transactions in this block.
    fn gas_used(&self) -> u64;

    /// Returns the total gas limit for this block.
    fn gas_limit(&self) -> u64;

    /// Returns the number of transactions in the payload.
    fn transaction_count(&self) -> usize;
    /// Returns the slot number included in this payload.
    ///
    /// Returns `None` for pre-Amsterdam blocks.
    fn slot_number(&self) -> Option<u64>;
}
