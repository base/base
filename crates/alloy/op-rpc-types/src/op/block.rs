//! Block RPC types.

#![allow(unknown_lints, non_local_definitions)]

use crate::op::transaction::Transaction;
use alloy::rpc::types::eth::Header;
use alloy_eips::{calc_blob_gasprice, calc_excess_blob_gas};
use alloy_primitives::{
    ruint::ParseError, Address, BlockHash, BlockNumber, Bloom, Bytes, B256, B64, U256, U64,
};
use alloy_rlp::{bytes, Decodable, Encodable, Error as RlpError};
use serde::{
    de::{MapAccess, Visitor},
    ser::{Error, SerializeStruct},
    Deserialize, Deserializer, Serialize, Serializer,
};
use std::{collections::BTreeMap, fmt, num::ParseIntError, ops::Deref, str::FromStr};

/// Block representation
#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    /// Header of the block.
    #[serde(flatten)]
    pub header: Header,
    /// Uncles' hashes.
    #[serde(default)]
    pub uncles: Vec<B256>,
    /// Block Transactions. In the case of an uncle block, this field is not included in RPC
    /// responses, and when deserialized, it will be set to [BlockTransactions::Uncle].
    #[serde(
        default = "BlockTransactions::uncle",
        skip_serializing_if = "BlockTransactions::is_uncle"
    )]
    pub transactions: BlockTransactions,
    /// Integer the size of this block in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<U256>,
    /// Withdrawals in the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawals: Option<Vec<Withdrawal>>,
}

impl Block {
    /// Converts a block with Tx hashes into a full block.
    pub fn into_full_block(self, txs: Vec<Transaction>) -> Self {
        Self { transactions: BlockTransactions::Full(txs), ..self }
    }
}

/// Block Transactions depending on the boolean attribute of `eth_getBlockBy*`,
/// or if used by `eth_getUncle*`
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BlockTransactions<T = Transaction> {
    /// Only hashes
    Hashes(Vec<B256>),
    /// Full transactions
    Full(Vec<T>),
    /// Special case for uncle response.
    Uncle,
}

impl Default for BlockTransactions {
    fn default() -> Self {
        BlockTransactions::Hashes(Vec::default())
    }
}

impl BlockTransactions {
    /// Converts `self` into `Hashes`.
    #[inline]
    pub fn convert_to_hashes(&mut self) {
        if !self.is_hashes() {
            *self = Self::Hashes(self.hashes().copied().collect());
        }
    }

    /// Converts `self` into `Hashes`.
    #[inline]
    pub fn into_hashes(mut self) -> Self {
        self.convert_to_hashes();
        self
    }

    /// Check if the enum variant is used for hashes.
    #[inline]
    pub const fn is_hashes(&self) -> bool {
        matches!(self, Self::Hashes(_))
    }

    /// Returns true if the enum variant is used for full transactions.
    #[inline]
    pub const fn is_full(&self) -> bool {
        matches!(self, Self::Full(_))
    }

    /// Returns true if the enum variant is used for an uncle response.
    #[inline]
    pub const fn is_uncle(&self) -> bool {
        matches!(self, Self::Uncle)
    }

    /// Returns an iterator over the transaction hashes.
    #[deprecated = "use `hashes` instead"]
    #[inline]
    pub fn iter(&self) -> BlockTransactionHashes<'_> {
        self.hashes()
    }

    /// Returns an iterator over references to the transaction hashes.
    #[inline]
    pub fn hashes(&self) -> BlockTransactionHashes<'_> {
        BlockTransactionHashes::new(self)
    }

    /// Returns an iterator over mutable references to the transaction hashes.
    #[inline]
    pub fn hashes_mut(&mut self) -> BlockTransactionHashesMut<'_> {
        BlockTransactionHashesMut::new(self)
    }

    /// Returns an instance of BlockTransactions with the Uncle special case.
    #[inline]
    pub const fn uncle() -> Self {
        Self::Uncle
    }

    /// Returns the number of transactions.
    #[inline]
    pub fn len(&self) -> usize {
        self.hashes().len()
    }

    /// Whether the block has no transactions.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An iterator over the transaction hashes of a block.
///
/// See [`BlockTransactions::hashes`].
#[derive(Clone, Debug)]
pub struct BlockTransactionHashes<'a>(BlockTransactionHashesInner<'a>);

#[derive(Clone, Debug)]
enum BlockTransactionHashesInner<'a> {
    Hashes(std::slice::Iter<'a, B256>),
    Full(std::slice::Iter<'a, Transaction>),
    Uncle,
}

impl<'a> BlockTransactionHashes<'a> {
    #[inline]
    fn new(txs: &'a BlockTransactions) -> Self {
        Self(match txs {
            BlockTransactions::Hashes(txs) => BlockTransactionHashesInner::Hashes(txs.iter()),
            BlockTransactions::Full(txs) => BlockTransactionHashesInner::Full(txs.iter()),
            BlockTransactions::Uncle => BlockTransactionHashesInner::Uncle,
        })
    }
}

impl<'a> Iterator for BlockTransactionHashes<'a> {
    type Item = &'a B256;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            BlockTransactionHashesInner::Full(txs) => txs.next().map(|tx| &tx.hash),
            BlockTransactionHashesInner::Hashes(txs) => txs.next(),
            BlockTransactionHashesInner::Uncle => None,
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            BlockTransactionHashesInner::Full(txs) => txs.size_hint(),
            BlockTransactionHashesInner::Hashes(txs) => txs.size_hint(),
            BlockTransactionHashesInner::Uncle => (0, Some(0)),
        }
    }
}

impl ExactSizeIterator for BlockTransactionHashes<'_> {
    #[inline]
    fn len(&self) -> usize {
        match &self.0 {
            BlockTransactionHashesInner::Full(txs) => txs.len(),
            BlockTransactionHashesInner::Hashes(txs) => txs.len(),
            BlockTransactionHashesInner::Uncle => 0,
        }
    }
}

impl DoubleEndedIterator for BlockTransactionHashes<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            BlockTransactionHashesInner::Full(txs) => txs.next_back().map(|tx| &tx.hash),
            BlockTransactionHashesInner::Hashes(txs) => txs.next_back(),
            BlockTransactionHashesInner::Uncle => None,
        }
    }
}

impl<'a> std::iter::FusedIterator for BlockTransactionHashes<'a> {}

/// An Iterator over the transaction hashes of a block.
///
/// See [`BlockTransactions::hashes_mut`].
#[derive(Debug)]
pub struct BlockTransactionHashesMut<'a>(BlockTransactionHashesInnerMut<'a>);

#[derive(Debug)]
enum BlockTransactionHashesInnerMut<'a> {
    Hashes(std::slice::IterMut<'a, B256>),
    Full(std::slice::IterMut<'a, Transaction>),
    Uncle,
}

impl<'a> BlockTransactionHashesMut<'a> {
    #[inline]
    fn new(txs: &'a mut BlockTransactions) -> Self {
        Self(match txs {
            BlockTransactions::Hashes(txs) => {
                BlockTransactionHashesInnerMut::Hashes(txs.iter_mut())
            }
            BlockTransactions::Full(txs) => BlockTransactionHashesInnerMut::Full(txs.iter_mut()),
            BlockTransactions::Uncle => BlockTransactionHashesInnerMut::Uncle,
        })
    }
}

impl<'a> Iterator for BlockTransactionHashesMut<'a> {
    type Item = &'a mut B256;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            BlockTransactionHashesInnerMut::Full(txs) => txs.next().map(|tx| &mut tx.hash),
            BlockTransactionHashesInnerMut::Hashes(txs) => txs.next(),
            BlockTransactionHashesInnerMut::Uncle => None,
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            BlockTransactionHashesInnerMut::Full(txs) => txs.size_hint(),
            BlockTransactionHashesInnerMut::Hashes(txs) => txs.size_hint(),
            BlockTransactionHashesInnerMut::Uncle => (0, Some(0)),
        }
    }
}

impl ExactSizeIterator for BlockTransactionHashesMut<'_> {
    #[inline]
    fn len(&self) -> usize {
        match &self.0 {
            BlockTransactionHashesInnerMut::Full(txs) => txs.len(),
            BlockTransactionHashesInnerMut::Hashes(txs) => txs.len(),
            BlockTransactionHashesInnerMut::Uncle => 0,
        }
    }
}

impl DoubleEndedIterator for BlockTransactionHashesMut<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            BlockTransactionHashesInnerMut::Full(txs) => txs.next_back().map(|tx| &mut tx.hash),
            BlockTransactionHashesInnerMut::Hashes(txs) => txs.next_back(),
            BlockTransactionHashesInnerMut::Uncle => None,
        }
    }
}

impl<'a> std::iter::FusedIterator for BlockTransactionHashesMut<'a> {}

/// Determines how the `transactions` field of [Block] should be filled.
///
/// This essentially represents the `full:bool` argument in RPC calls that determine whether the
/// response should include full transaction objects or just the hashes.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum BlockTransactionsKind {
    /// Only include hashes: [BlockTransactions::Hashes]
    Hashes,
    /// Include full transaction objects: [BlockTransactions::Full]
    Full,
}

impl From<bool> for BlockTransactionsKind {
    fn from(is_full: bool) -> Self {
        if is_full {
            BlockTransactionsKind::Full
        } else {
            BlockTransactionsKind::Hashes
        }
    }
}

/// Error that can occur when converting other types to blocks
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum BlockError {
    /// A transaction failed sender recovery
    #[error("transaction failed sender recovery")]
    InvalidSignature,
    /// A raw block failed to decode
    #[error("failed to decode raw block {0}")]
    RlpDecodeRawBlock(alloy_rlp::Error),
}

/// A block hash which may have
/// a boolean requireCanonical field.
/// If false, an RPC call should raise if a block
/// matching the hash is not found.
/// If true, an RPC call should additionally raise if
/// the block is not in the canonical chain.
/// <https://github.com/ethereum/EIPs/blob/master/EIPS/eip-1898.md#specification>
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RpcBlockHash {
    /// A block hash
    pub block_hash: B256,
    /// Whether the block must be a canonical block
    pub require_canonical: Option<bool>,
}

impl RpcBlockHash {
    /// Returns an [RpcBlockHash] from a [B256].
    pub const fn from_hash(block_hash: B256, require_canonical: Option<bool>) -> Self {
        RpcBlockHash { block_hash, require_canonical }
    }
}

impl From<B256> for RpcBlockHash {
    fn from(value: B256) -> Self {
        Self::from_hash(value, None)
    }
}

impl From<RpcBlockHash> for B256 {
    fn from(value: RpcBlockHash) -> Self {
        value.block_hash
    }
}

impl AsRef<B256> for RpcBlockHash {
    fn as_ref(&self) -> &B256 {
        &self.block_hash
    }
}

/// A block Number (or tag - "latest", "earliest", "pending")
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum BlockNumberOrTag {
    /// Latest block
    #[default]
    Latest,
    /// Finalized block accepted as canonical
    Finalized,
    /// Safe head block
    Safe,
    /// Earliest block (genesis)
    Earliest,
    /// Pending block (not yet part of the blockchain)
    Pending,
    /// Block by number from canon chain
    Number(u64),
}

impl BlockNumberOrTag {
    /// Returns the numeric block number if explicitly set
    pub const fn as_number(&self) -> Option<u64> {
        match *self {
            BlockNumberOrTag::Number(num) => Some(num),
            _ => None,
        }
    }

    /// Returns `true` if a numeric block number is set
    pub const fn is_number(&self) -> bool {
        matches!(self, BlockNumberOrTag::Number(_))
    }

    /// Returns `true` if it's "latest"
    pub const fn is_latest(&self) -> bool {
        matches!(self, BlockNumberOrTag::Latest)
    }

    /// Returns `true` if it's "finalized"
    pub const fn is_finalized(&self) -> bool {
        matches!(self, BlockNumberOrTag::Finalized)
    }

    /// Returns `true` if it's "safe"
    pub const fn is_safe(&self) -> bool {
        matches!(self, BlockNumberOrTag::Safe)
    }

    /// Returns `true` if it's "pending"
    pub const fn is_pending(&self) -> bool {
        matches!(self, BlockNumberOrTag::Pending)
    }

    /// Returns `true` if it's "earliest"
    pub const fn is_earliest(&self) -> bool {
        matches!(self, BlockNumberOrTag::Earliest)
    }
}

impl From<u64> for BlockNumberOrTag {
    fn from(num: u64) -> Self {
        BlockNumberOrTag::Number(num)
    }
}

impl From<U64> for BlockNumberOrTag {
    fn from(num: U64) -> Self {
        num.to::<u64>().into()
    }
}

impl Serialize for BlockNumberOrTag {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *self {
            BlockNumberOrTag::Number(x) => serializer.serialize_str(&format!("0x{x:x}")),
            BlockNumberOrTag::Latest => serializer.serialize_str("latest"),
            BlockNumberOrTag::Finalized => serializer.serialize_str("finalized"),
            BlockNumberOrTag::Safe => serializer.serialize_str("safe"),
            BlockNumberOrTag::Earliest => serializer.serialize_str("earliest"),
            BlockNumberOrTag::Pending => serializer.serialize_str("pending"),
        }
    }
}

impl<'de> Deserialize<'de> for BlockNumberOrTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?.to_lowercase();
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl FromStr for BlockNumberOrTag {
    type Err = ParseBlockNumberError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let block = match s {
            "latest" => Self::Latest,
            "finalized" => Self::Finalized,
            "safe" => Self::Safe,
            "earliest" => Self::Earliest,
            "pending" => Self::Pending,
            _number => {
                if let Some(hex_val) = s.strip_prefix("0x") {
                    let number = u64::from_str_radix(hex_val, 16);
                    BlockNumberOrTag::Number(number?)
                } else {
                    return Err(HexStringMissingPrefixError::default().into());
                }
            }
        };
        Ok(block)
    }
}

impl fmt::Display for BlockNumberOrTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockNumberOrTag::Number(x) => write!(f, "0x{x:x}"),
            BlockNumberOrTag::Latest => f.write_str("latest"),
            BlockNumberOrTag::Finalized => f.write_str("finalized"),
            BlockNumberOrTag::Safe => f.write_str("safe"),
            BlockNumberOrTag::Earliest => f.write_str("earliest"),
            BlockNumberOrTag::Pending => f.write_str("pending"),
        }
    }
}

/// Error variants when parsing a [BlockNumberOrTag]
#[derive(Debug, thiserror::Error)]
pub enum ParseBlockNumberError {
    /// Failed to parse hex value
    #[error(transparent)]
    ParseIntErr(#[from] ParseIntError),
    /// Failed to parse hex value
    #[error(transparent)]
    ParseErr(#[from] ParseError),
    /// Block numbers should be 0x-prefixed
    #[error(transparent)]
    MissingPrefix(#[from] HexStringMissingPrefixError),
}

/// Thrown when a 0x-prefixed hex string was expected
#[derive(Copy, Clone, Debug, Default, thiserror::Error)]
#[non_exhaustive]
#[error("hex string without 0x prefix")]
pub struct HexStringMissingPrefixError;

/// A Block Identifier
/// <https://github.com/ethereum/EIPs/blob/master/EIPS/eip-1898.md>
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockId {
    /// A block hash and an optional bool that defines if it's canonical
    Hash(RpcBlockHash),
    /// A block number
    Number(BlockNumberOrTag),
}

// === impl BlockId ===

impl BlockId {
    /// Returns the block hash if it is [BlockId::Hash]
    pub const fn as_block_hash(&self) -> Option<B256> {
        match self {
            BlockId::Hash(hash) => Some(hash.block_hash),
            BlockId::Number(_) => None,
        }
    }

    /// Returns true if this is [BlockNumberOrTag::Latest]
    pub const fn is_latest(&self) -> bool {
        matches!(self, BlockId::Number(BlockNumberOrTag::Latest))
    }

    /// Returns true if this is [BlockNumberOrTag::Pending]
    pub const fn is_pending(&self) -> bool {
        matches!(self, BlockId::Number(BlockNumberOrTag::Pending))
    }

    /// Returns true if this is [BlockNumberOrTag::Safe]
    pub const fn is_safe(&self) -> bool {
        matches!(self, BlockId::Number(BlockNumberOrTag::Safe))
    }

    /// Returns true if this is [BlockNumberOrTag::Finalized]
    pub const fn is_finalized(&self) -> bool {
        matches!(self, BlockId::Number(BlockNumberOrTag::Finalized))
    }

    /// Returns true if this is [BlockNumberOrTag::Earliest]
    pub const fn is_earliest(&self) -> bool {
        matches!(self, BlockId::Number(BlockNumberOrTag::Earliest))
    }

    /// Returns true if this is [BlockNumberOrTag::Number]
    pub const fn is_number(&self) -> bool {
        matches!(self, BlockId::Number(BlockNumberOrTag::Number(_)))
    }
    /// Returns true if this is [BlockId::Hash]
    pub const fn is_hash(&self) -> bool {
        matches!(self, BlockId::Hash(_))
    }

    /// Creates a new "pending" tag instance.
    pub const fn pending() -> Self {
        BlockId::Number(BlockNumberOrTag::Pending)
    }

    /// Creates a new "latest" tag instance.
    pub const fn latest() -> Self {
        BlockId::Number(BlockNumberOrTag::Latest)
    }

    /// Creates a new "earliest" tag instance.
    pub const fn earliest() -> Self {
        BlockId::Number(BlockNumberOrTag::Earliest)
    }

    /// Creates a new "finalized" tag instance.
    pub const fn finalized() -> Self {
        BlockId::Number(BlockNumberOrTag::Finalized)
    }

    /// Creates a new "safe" tag instance.
    pub const fn safe() -> Self {
        BlockId::Number(BlockNumberOrTag::Safe)
    }

    /// Creates a new block number instance.
    pub const fn number(num: u64) -> Self {
        BlockId::Number(BlockNumberOrTag::Number(num))
    }

    /// Create a new block hash instance.
    pub const fn hash(block_hash: B256) -> Self {
        BlockId::Hash(RpcBlockHash { block_hash, require_canonical: None })
    }

    /// Create a new block hash instance that requires the block to be canonical.
    pub const fn hash_canonical(block_hash: B256) -> Self {
        BlockId::Hash(RpcBlockHash { block_hash, require_canonical: Some(true) })
    }
}

impl Default for BlockId {
    fn default() -> Self {
        BlockId::Number(BlockNumberOrTag::Latest)
    }
}

impl From<u64> for BlockId {
    fn from(num: u64) -> Self {
        BlockNumberOrTag::Number(num).into()
    }
}

impl From<U64> for BlockId {
    fn from(value: U64) -> Self {
        BlockNumberOrTag::Number(value.to()).into()
    }
}

impl From<BlockNumberOrTag> for BlockId {
    fn from(num: BlockNumberOrTag) -> Self {
        BlockId::Number(num)
    }
}

impl From<B256> for BlockId {
    fn from(block_hash: B256) -> Self {
        BlockId::Hash(RpcBlockHash { block_hash, require_canonical: None })
    }
}

impl From<(B256, Option<bool>)> for BlockId {
    fn from(hash_can: (B256, Option<bool>)) -> Self {
        BlockId::Hash(RpcBlockHash { block_hash: hash_can.0, require_canonical: hash_can.1 })
    }
}

impl Serialize for BlockId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            BlockId::Hash(RpcBlockHash { block_hash, require_canonical }) => {
                let mut s = serializer.serialize_struct("BlockIdEip1898", 1)?;
                s.serialize_field("blockHash", block_hash)?;
                if let Some(require_canonical) = require_canonical {
                    s.serialize_field("requireCanonical", require_canonical)?;
                }
                s.end()
            }
            BlockId::Number(num) => num.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for BlockId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BlockIdVisitor;

        impl<'de> Visitor<'de> for BlockIdVisitor {
            type Value = BlockId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("Block identifier following EIP-1898")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                // Since there is no way to clearly distinguish between a DATA parameter and a QUANTITY parameter. A str is therefor deserialized into a Block Number: <https://github.com/ethereum/EIPs/blob/master/EIPS/eip-1898.md>
                // However, since the hex string should be a QUANTITY, we can safely assume that if the len is 66 bytes, it is in fact a hash, ref <https://github.com/ethereum/go-ethereum/blob/ee530c0d5aa70d2c00ab5691a89ab431b73f8165/rpc/types.go#L184-L184>
                if v.len() == 66 {
                    Ok(BlockId::Hash(v.parse::<B256>().map_err(serde::de::Error::custom)?.into()))
                } else {
                    // quantity hex string or tag
                    Ok(BlockId::Number(v.parse().map_err(serde::de::Error::custom)?))
                }
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut number = None;
                let mut block_hash = None;
                let mut require_canonical = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "blockNumber" => {
                            if number.is_some() || block_hash.is_some() {
                                return Err(serde::de::Error::duplicate_field("blockNumber"));
                            }
                            if require_canonical.is_some() {
                                return Err(serde::de::Error::custom(
                                    "Non-valid require_canonical field",
                                ));
                            }
                            number = Some(map.next_value::<BlockNumberOrTag>()?)
                        }
                        "blockHash" => {
                            if number.is_some() || block_hash.is_some() {
                                return Err(serde::de::Error::duplicate_field("blockHash"));
                            }

                            block_hash = Some(map.next_value::<B256>()?);
                        }
                        "requireCanonical" => {
                            if number.is_some() || require_canonical.is_some() {
                                return Err(serde::de::Error::duplicate_field("requireCanonical"));
                            }

                            require_canonical = Some(map.next_value::<bool>()?)
                        }
                        key => {
                            return Err(serde::de::Error::unknown_field(
                                key,
                                &["blockNumber", "blockHash", "requireCanonical"],
                            ))
                        }
                    }
                }

                if let Some(number) = number {
                    Ok(BlockId::Number(number))
                } else if let Some(block_hash) = block_hash {
                    Ok(BlockId::Hash(RpcBlockHash { block_hash, require_canonical }))
                } else {
                    Err(serde::de::Error::custom(
                        "Expected `blockNumber` or `blockHash` with `requireCanonical` optionally",
                    ))
                }
            }
        }

        deserializer.deserialize_any(BlockIdVisitor)
    }
}

/// Error thrown when parsing a [BlockId] from a string.
#[derive(Debug, thiserror::Error)]
pub enum ParseBlockIdError {
    /// Failed to parse a block id from a number.
    #[error(transparent)]
    ParseIntError(#[from] ParseIntError),
    /// Failed to parse a block id as a hex string.
    #[error(transparent)]
    FromHexError(#[from] alloy_primitives::hex::FromHexError),
}

impl FromStr for BlockId {
    type Err = ParseBlockIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with("0x") {
            return B256::from_str(s).map(Into::into).map_err(ParseBlockIdError::FromHexError);
        }

        match s {
            "latest" => Ok(BlockId::Number(BlockNumberOrTag::Latest)),
            "finalized" => Ok(BlockId::Number(BlockNumberOrTag::Finalized)),
            "safe" => Ok(BlockId::Number(BlockNumberOrTag::Safe)),
            "earliest" => Ok(BlockId::Number(BlockNumberOrTag::Earliest)),
            "pending" => Ok(BlockId::Number(BlockNumberOrTag::Pending)),
            _ => s
                .parse::<u64>()
                .map_err(ParseBlockIdError::ParseIntError)
                .map(|n| BlockId::Number(n.into())),
        }
    }
}

/// Block number and hash.
#[derive(Clone, Copy, Hash, Default, PartialEq, Eq)]
pub struct BlockNumHash {
    /// Block number
    pub number: BlockNumber,
    /// Block hash
    pub hash: BlockHash,
}

/// Block number and hash of the forked block.
pub type ForkBlock = BlockNumHash;

impl fmt::Debug for BlockNumHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("").field(&self.number).field(&self.hash).finish()
    }
}

impl BlockNumHash {
    /// Creates a new `BlockNumHash` from a block number and hash.
    pub const fn new(number: BlockNumber, hash: BlockHash) -> Self {
        Self { number, hash }
    }

    /// Consumes `Self` and returns [`BlockNumber`], [`BlockHash`]
    pub const fn into_components(self) -> (BlockNumber, BlockHash) {
        (self.number, self.hash)
    }

    /// Returns whether or not the block matches the given [BlockHashOrNumber].
    pub fn matches_block_or_num(&self, block: &BlockHashOrNumber) -> bool {
        match block {
            BlockHashOrNumber::Hash(hash) => self.hash == *hash,
            BlockHashOrNumber::Number(number) => self.number == *number,
        }
    }
}

impl From<(BlockNumber, BlockHash)> for BlockNumHash {
    fn from(val: (BlockNumber, BlockHash)) -> Self {
        BlockNumHash { number: val.0, hash: val.1 }
    }
}

impl From<(BlockHash, BlockNumber)> for BlockNumHash {
    fn from(val: (BlockHash, BlockNumber)) -> Self {
        BlockNumHash { hash: val.0, number: val.1 }
    }
}

/// Either a block hash _or_ a block number
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(
    any(test, feature = "arbitrary"),
    derive(proptest_derive::Arbitrary, arbitrary::Arbitrary)
)]
pub enum BlockHashOrNumber {
    /// A block hash
    Hash(B256),
    /// A block number
    Number(u64),
}

// === impl BlockHashOrNumber ===

impl BlockHashOrNumber {
    /// Returns the block number if it is a [`BlockHashOrNumber::Number`].
    #[inline]
    pub const fn as_number(self) -> Option<u64> {
        match self {
            BlockHashOrNumber::Hash(_) => None,
            BlockHashOrNumber::Number(num) => Some(num),
        }
    }
}

impl From<B256> for BlockHashOrNumber {
    fn from(value: B256) -> Self {
        BlockHashOrNumber::Hash(value)
    }
}

impl From<u64> for BlockHashOrNumber {
    fn from(value: u64) -> Self {
        BlockHashOrNumber::Number(value)
    }
}

impl From<U64> for BlockHashOrNumber {
    fn from(value: U64) -> Self {
        value.to::<u64>().into()
    }
}

/// Allows for RLP encoding of either a block hash or block number
impl Encodable for BlockHashOrNumber {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            Self::Hash(block_hash) => block_hash.encode(out),
            Self::Number(block_number) => block_number.encode(out),
        }
    }
    fn length(&self) -> usize {
        match self {
            Self::Hash(block_hash) => block_hash.length(),
            Self::Number(block_number) => block_number.length(),
        }
    }
}

/// Allows for RLP decoding of a block hash or block number
impl Decodable for BlockHashOrNumber {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header: u8 = *buf.first().ok_or(RlpError::InputTooShort)?;
        // if the byte string is exactly 32 bytes, decode it into a Hash
        // 0xa0 = 0x80 (start of string) + 0x20 (32, length of string)
        if header == 0xa0 {
            // strip the first byte, parsing the rest of the string.
            // If the rest of the string fails to decode into 32 bytes, we'll bubble up the
            // decoding error.
            let hash = B256::decode(buf)?;
            Ok(Self::Hash(hash))
        } else {
            // a block number when encoded as bytes ranges from 0 to any number of bytes - we're
            // going to accept numbers which fit in less than 64 bytes.
            // Any data larger than this which is not caught by the Hash decoding should error and
            // is considered an invalid block number.
            Ok(Self::Number(u64::decode(buf)?))
        }
    }
}

/// Error thrown when parsing a [BlockHashOrNumber] from a string.
#[derive(Debug, thiserror::Error)]
#[error("failed to parse {input:?} as a number: {parse_int_error} or hash: {hex_error}")]
pub struct ParseBlockHashOrNumberError {
    input: String,
    parse_int_error: ParseIntError,
    hex_error: alloy_primitives::hex::FromHexError,
}

impl FromStr for BlockHashOrNumber {
    type Err = ParseBlockHashOrNumberError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match u64::from_str(s) {
            Ok(val) => Ok(val.into()),
            Err(pares_int_error) => match B256::from_str(s) {
                Ok(val) => Ok(val.into()),
                Err(hex_error) => Err(ParseBlockHashOrNumberError {
                    input: s.to_string(),
                    parse_int_error: pares_int_error,
                    hex_error,
                }),
            },
        }
    }
}

/// A Block representation that allows to include additional fields
pub type RichBlock = Rich<Block>;

impl From<Block> for RichBlock {
    fn from(block: Block) -> Self {
        Rich { inner: block, extra_info: Default::default() }
    }
}

/// Header representation with additional info.
pub type RichHeader = Rich<Header>;

impl From<Header> for RichHeader {
    fn from(header: Header) -> Self {
        Rich { inner: header, extra_info: Default::default() }
    }
}

/// Value representation with additional info
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Rich<T> {
    /// Standard value.
    #[serde(flatten)]
    pub inner: T,
    /// Additional fields that should be serialized into the `Block` object
    #[serde(flatten)]
    pub extra_info: BTreeMap<String, serde_json::Value>,
}

impl<T> Deref for Rich<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: Serialize> Serialize for Rich<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.extra_info.is_empty() {
            return self.inner.serialize(serializer);
        }

        let inner = serde_json::to_value(&self.inner);
        let extras = serde_json::to_value(&self.extra_info);

        if let (Ok(serde_json::Value::Object(mut value)), Ok(serde_json::Value::Object(extras))) =
            (inner, extras)
        {
            value.extend(extras);
            value.serialize(serializer)
        } else {
            Err(S::Error::custom("Unserializable structures: expected objects"))
        }
    }
}

/// BlockOverrides is a set of header fields to override.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct BlockOverrides {
    /// Overrides the block number.
    ///
    /// For `eth_callMany` this will be the block number of the first simulated block. Each
    /// following block increments its block number by 1
    // Note: geth uses `number`, erigon uses `blockNumber`
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "blockNumber")]
    pub number: Option<U256>,
    /// Overrides the difficulty of the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<U256>,
    /// Overrides the timestamp of the block.
    // Note: geth uses `time`, erigon uses `timestamp`
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "timestamp")]
    pub time: Option<U64>,
    /// Overrides the gas limit of the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gas_limit: Option<U64>,
    /// Overrides the coinbase address of the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coinbase: Option<Address>,
    /// Overrides the prevrandao of the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub random: Option<B256>,
    /// Overrides the basefee of the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_fee: Option<U256>,
    /// A dictionary that maps blockNumber to a user-defined hash. It could be queried from the
    /// solidity opcode BLOCKHASH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<BTreeMap<u64, B256>>,
}
