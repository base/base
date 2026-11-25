//! Block data and the canonical value encodings of the block families (`0x10`–`0x15`).
//!
//! The encodings are canonical Ethereum/OP encodings, not reth's `Compact` rows, so they are
//! stable across reth upgrades and pinned by the format byte:
//!
//! - header (`0x10`): RLP `Header`.
//! - body (`0x11`): RLP list `[txs, withdrawals?]`, where `txs` is an RLP list header followed by
//!   each transaction's EIP-2718 bytes (typed transactions are not wrapped in an RLP string) and
//!   `withdrawals` is the RLP withdrawal list, present only when the block has one.
//! - receipts (`0x12`): RLP list header followed by each receipt's EIP-2718 `ReceiptWithBloom`
//!   bytes, deposit nonce and version included.
//! - senders (`0x13`): concatenated 20-byte addresses.
//! - account changes (`0x14`): sorted by address, `address[20] ‖ present:u8 ‖ nonce:u64be ‖
//!   balance:u256be ‖ code_hash[32]`; an account that did not exist is `present = 0` with zeroed
//!   fields; an account without code has `code_hash = KECCAK_EMPTY`.
//! - storage changes (`0x15`): sorted by `(address, slot)`, `address[20] ‖ slot[32] ‖
//!   before:u256be`.

use alloy_consensus::{Header, TxReceipt};
use alloy_eips::{
    eip2718::{Decodable2718, Encodable2718},
    eip4895::Withdrawals,
};
use alloy_primitives::{Address, B256, U256};
use alloy_rlp::{Decodable, Encodable};
use base_common_consensus::{BaseReceipt, BaseReceiptEnvelope, BaseTxEnvelope};

use crate::{ArchiveError, Family};

/// Encoded width of one account change.
pub const ACCOUNT_CHANGE_LEN: usize = 20 + 1 + 8 + 32 + 32;

/// Encoded width of one storage change.
pub const STORAGE_CHANGE_LEN: usize = 20 + 32 + 32;

/// Account fields kept in the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountInfo {
    /// Nonce.
    pub nonce: u64,
    /// Balance.
    pub balance: U256,
    /// Code hash, `KECCAK_EMPTY` for accounts without code.
    pub code_hash: B256,
}

/// An account's value before a block changed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountChange {
    /// Account address.
    pub address: Address,
    /// Value before the block, `None` if the account did not exist.
    pub before: Option<AccountInfo>,
}

/// A storage slot's value before a block changed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageChange {
    /// Account address.
    pub address: Address,
    /// Slot.
    pub slot: B256,
    /// Value before the block.
    pub before: U256,
}

/// A block body as stored in family `0x11`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockBody {
    /// Transactions in block order.
    pub transactions: Vec<BaseTxEnvelope>,
    /// Withdrawals, as the node stores them (`None` before Canyon, empty after).
    pub withdrawals: Option<Withdrawals>,
}

/// Everything the archive stores for one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockData {
    /// Header.
    pub header: Header,
    /// Body.
    pub body: BlockBody,
    /// Sender of each transaction.
    pub senders: Vec<Address>,
    /// Receipt of each transaction.
    pub receipts: Vec<BaseReceipt>,
    /// Account before-values, any order.
    pub account_changes: Vec<AccountChange>,
    /// Storage before-values, any order.
    pub storage_changes: Vec<StorageChange>,
}

impl BlockData {
    /// Block number.
    pub const fn number(&self) -> u64 {
        self.header.number
    }

    /// Block hash computed from the header.
    pub fn hash(&self) -> B256 {
        self.header.hash_slow()
    }
}

/// Encoders and decoders for block-family values.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlockCodec;

impl BlockCodec {
    /// Encodes a header.
    pub fn encode_header(header: &Header) -> Vec<u8> {
        alloy_rlp::encode(header)
    }

    /// Decodes a header, rejecting trailing bytes.
    pub fn decode_header(raw: &[u8]) -> Result<Header, ArchiveError> {
        let mut buf = raw;
        let header =
            Header::decode(&mut buf).map_err(|e| ArchiveError::decode(Family::Header, e))?;
        if !buf.is_empty() {
            return Err(ArchiveError::decode(Family::Header, "trailing bytes"));
        }
        Ok(header)
    }

    /// Writes an RLP list header followed by each item's EIP-2718 bytes.
    pub fn encode_2718_list<T: Encodable2718>(items: &[T], out: &mut Vec<u8>) {
        let payload_length = items.iter().map(Encodable2718::encode_2718_len).sum();
        alloy_rlp::Header { list: true, payload_length }.encode(out);
        for item in items {
            item.encode_2718(out);
        }
    }

    /// Reads a list written by [`Self::encode_2718_list`].
    pub fn decode_2718_list<T: Decodable2718>(
        buf: &mut &[u8],
        family: Family,
    ) -> Result<Vec<T>, ArchiveError> {
        let header = alloy_rlp::Header::decode(buf).map_err(|e| ArchiveError::decode(family, e))?;
        if !header.list || header.payload_length > buf.len() {
            return Err(ArchiveError::decode(family, "expected a list"));
        }
        let (mut payload, rest) = buf.split_at(header.payload_length);
        *buf = rest;
        let mut items = Vec::new();
        while !payload.is_empty() {
            items.push(T::decode_2718(&mut payload).map_err(|e| ArchiveError::decode(family, e))?);
        }
        Ok(items)
    }

    /// Encodes a body.
    pub fn encode_body(body: &BlockBody) -> Vec<u8> {
        let mut inner = Vec::new();
        Self::encode_2718_list(&body.transactions, &mut inner);
        if let Some(withdrawals) = &body.withdrawals {
            withdrawals.encode(&mut inner);
        }
        let mut out = Vec::with_capacity(inner.len() + 9);
        alloy_rlp::Header { list: true, payload_length: inner.len() }.encode(&mut out);
        out.extend_from_slice(&inner);
        out
    }

    /// Decodes a body.
    pub fn decode_body(raw: &[u8]) -> Result<BlockBody, ArchiveError> {
        let family = Family::Body;
        let mut buf = raw;
        let header =
            alloy_rlp::Header::decode(&mut buf).map_err(|e| ArchiveError::decode(family, e))?;
        if !header.list || header.payload_length != buf.len() {
            return Err(ArchiveError::decode(family, "expected one list"));
        }
        let transactions = Self::decode_2718_list(&mut buf, family)?;
        let withdrawals = if buf.is_empty() {
            None
        } else {
            Some(Withdrawals::decode(&mut buf).map_err(|e| ArchiveError::decode(family, e))?)
        };
        if !buf.is_empty() {
            return Err(ArchiveError::decode(family, "trailing bytes"));
        }
        Ok(BlockBody { transactions, withdrawals })
    }

    /// Encodes receipts.
    pub fn encode_receipts(receipts: &[BaseReceipt]) -> Vec<u8> {
        let with_bloom: Vec<_> = receipts.iter().map(TxReceipt::with_bloom_ref).collect();
        let mut out = Vec::new();
        Self::encode_2718_list(&with_bloom, &mut out);
        out
    }

    /// Decodes receipts.
    pub fn decode_receipts(raw: &[u8]) -> Result<Vec<BaseReceipt>, ArchiveError> {
        let mut buf = raw;
        let receipts: Vec<BaseReceiptEnvelope> =
            Self::decode_2718_list(&mut buf, Family::Receipts)?;
        if !buf.is_empty() {
            return Err(ArchiveError::decode(Family::Receipts, "trailing bytes"));
        }
        Ok(receipts.into_iter().map(BaseReceipt::from).collect())
    }

    /// Encodes senders.
    pub fn encode_senders(senders: &[Address]) -> Vec<u8> {
        senders.iter().flat_map(|sender| sender.into_array()).collect()
    }

    /// Decodes senders.
    pub fn decode_senders(raw: &[u8]) -> Result<Vec<Address>, ArchiveError> {
        if !raw.len().is_multiple_of(20) {
            return Err(ArchiveError::decode(Family::Senders, "length is not a multiple of 20"));
        }
        Ok(raw.chunks_exact(20).map(Address::from_slice).collect())
    }

    /// Encodes account changes, sorting them by address. Duplicate addresses are an error:
    /// changesets are keyed by address.
    pub fn encode_account_changes(
        block: u64,
        changes: &[AccountChange],
    ) -> Result<Vec<u8>, ArchiveError> {
        let mut sorted = changes.to_vec();
        sorted.sort_by_key(|change| change.address);
        if let Some(dup) = sorted.windows(2).find(|w| w[0].address == w[1].address) {
            return Err(ArchiveError::InvalidBlock {
                block,
                reason: format!("duplicate account change for {}", dup[0].address),
            });
        }
        let mut out = Vec::with_capacity(sorted.len() * ACCOUNT_CHANGE_LEN);
        for change in &sorted {
            out.extend_from_slice(change.address.as_slice());
            match change.before {
                Some(info) => {
                    out.push(1);
                    out.extend_from_slice(&info.nonce.to_be_bytes());
                    out.extend_from_slice(&info.balance.to_be_bytes::<32>());
                    out.extend_from_slice(info.code_hash.as_slice());
                }
                None => out.extend_from_slice(&[0u8; ACCOUNT_CHANGE_LEN - 20]),
            }
        }
        Ok(out)
    }

    /// Decodes account changes, requiring strictly increasing addresses and canonical absent
    /// entries.
    pub fn decode_account_changes(raw: &[u8]) -> Result<Vec<AccountChange>, ArchiveError> {
        let family = Family::AccountChanges;
        if !raw.len().is_multiple_of(ACCOUNT_CHANGE_LEN) {
            return Err(ArchiveError::decode(family, "bad length"));
        }
        let mut out: Vec<AccountChange> = Vec::with_capacity(raw.len() / ACCOUNT_CHANGE_LEN);
        for entry in raw.chunks_exact(ACCOUNT_CHANGE_LEN) {
            let address = Address::from_slice(&entry[..20]);
            if out.last().is_some_and(|prev| prev.address >= address) {
                return Err(ArchiveError::decode(family, "addresses not strictly increasing"));
            }
            let before = match entry[20] {
                0 if entry[21..].iter().all(|b| *b == 0) => None,
                1 => Some(AccountInfo {
                    nonce: u64::from_be_bytes(entry[21..29].try_into().expect("8 bytes")),
                    balance: U256::from_be_slice(&entry[29..61]),
                    code_hash: B256::from_slice(&entry[61..93]),
                }),
                _ => return Err(ArchiveError::decode(family, "bad presence byte")),
            };
            out.push(AccountChange { address, before });
        }
        Ok(out)
    }

    /// Encodes storage changes, sorting them by `(address, slot)`. Duplicates are an error.
    pub fn encode_storage_changes(
        block: u64,
        changes: &[StorageChange],
    ) -> Result<Vec<u8>, ArchiveError> {
        let mut sorted = changes.to_vec();
        sorted.sort_by_key(|change| (change.address, change.slot));
        if let Some(dup) =
            sorted.windows(2).find(|w| (w[0].address, w[0].slot) == (w[1].address, w[1].slot))
        {
            return Err(ArchiveError::InvalidBlock {
                block,
                reason: format!("duplicate storage change for {} {}", dup[0].address, dup[0].slot),
            });
        }
        let mut out = Vec::with_capacity(sorted.len() * STORAGE_CHANGE_LEN);
        for change in &sorted {
            out.extend_from_slice(change.address.as_slice());
            out.extend_from_slice(change.slot.as_slice());
            out.extend_from_slice(&change.before.to_be_bytes::<32>());
        }
        Ok(out)
    }

    /// Decodes storage changes, requiring strictly increasing `(address, slot)`.
    pub fn decode_storage_changes(raw: &[u8]) -> Result<Vec<StorageChange>, ArchiveError> {
        let family = Family::StorageChanges;
        if !raw.len().is_multiple_of(STORAGE_CHANGE_LEN) {
            return Err(ArchiveError::decode(family, "bad length"));
        }
        let mut out: Vec<StorageChange> = Vec::with_capacity(raw.len() / STORAGE_CHANGE_LEN);
        for entry in raw.chunks_exact(STORAGE_CHANGE_LEN) {
            let change = StorageChange {
                address: Address::from_slice(&entry[..20]),
                slot: B256::from_slice(&entry[20..52]),
                before: U256::from_be_slice(&entry[52..84]),
            };
            if out
                .last()
                .is_some_and(|prev| (prev.address, prev.slot) >= (change.address, change.slot))
            {
                return Err(ArchiveError::decode(family, "keys not strictly increasing"));
            }
            out.push(change);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::constants::KECCAK_EMPTY;

    use super::*;
    use crate::test_utils::ChainFixture;

    /// Every value of every fixture block round-trips through its canonical encoding, on both
    /// sides of the Canyon fork (withdrawals `None` before, empty after).
    #[test]
    fn block_values_roundtrip() {
        let fixture = ChainFixture::standard();
        for block in &fixture.blocks {
            let n = block.number();
            assert_eq!(
                BlockCodec::decode_header(&BlockCodec::encode_header(&block.header)).unwrap(),
                block.header,
                "header {n}"
            );
            assert_eq!(
                BlockCodec::decode_body(&BlockCodec::encode_body(&block.body)).unwrap(),
                block.body,
                "body {n}"
            );
            assert_eq!(
                BlockCodec::decode_receipts(&BlockCodec::encode_receipts(&block.receipts)).unwrap(),
                block.receipts,
                "receipts {n}"
            );
            assert_eq!(
                BlockCodec::decode_senders(&BlockCodec::encode_senders(&block.senders)).unwrap(),
                block.senders,
                "senders {n}"
            );
            let mut accounts = block.account_changes.clone();
            accounts.sort_by_key(|c| c.address);
            assert_eq!(
                BlockCodec::decode_account_changes(
                    &BlockCodec::encode_account_changes(n, &block.account_changes).unwrap()
                )
                .unwrap(),
                accounts,
                "account changes {n}"
            );
            let mut storage = block.storage_changes.clone();
            storage.sort_by_key(|c| (c.address, c.slot));
            assert_eq!(
                BlockCodec::decode_storage_changes(
                    &BlockCodec::encode_storage_changes(n, &block.storage_changes).unwrap()
                )
                .unwrap(),
                storage,
                "storage changes {n}"
            );
        }
        assert_eq!(fixture.blocks[12].body.withdrawals, None, "pre-Canyon body has no withdrawals");
        assert_eq!(
            fixture.blocks[13].body.withdrawals,
            Some(Withdrawals::default()),
            "post-Canyon body has an empty list"
        );
    }

    /// Typed transactions sit in the list as raw EIP-2718 bytes, not wrapped in an RLP string,
    /// and the receipt list carries the deposit nonce.
    #[test]
    fn typed_transactions_are_not_string_wrapped() {
        let fixture = ChainFixture::standard();
        let block = &fixture.blocks[1];
        let encoded = BlockCodec::encode_body(&block.body);
        let mut buf = encoded.as_slice();
        let outer = alloy_rlp::Header::decode(&mut buf).unwrap();
        assert!(outer.list, "body is a list");
        let txs = alloy_rlp::Header::decode(&mut buf).unwrap();
        assert!(txs.list, "transactions are a list");
        assert_eq!(
            buf[0], 0x7e,
            "first transaction starts with the deposit type byte, no string header"
        );
        assert_eq!(
            txs.payload_length,
            block.body.transactions.iter().map(Encodable2718::encode_2718_len).sum::<usize>(),
            "list payload is the concatenated 2718 encodings"
        );
        let receipts =
            BlockCodec::decode_receipts(&BlockCodec::encode_receipts(&block.receipts)).unwrap();
        let BaseReceipt::Deposit(deposit) = &receipts[0] else {
            panic!("first receipt is a deposit")
        };
        assert_eq!(deposit.deposit_nonce, Some(0), "deposit nonce survives the archive encoding");
    }

    /// An absent account uses the zeroed sentinel; duplicate keys are rejected.
    #[test]
    fn account_sentinel_and_duplicates() {
        let absent = AccountChange { address: Address::with_last_byte(1), before: None };
        let present = AccountChange {
            address: Address::with_last_byte(2),
            before: Some(AccountInfo { nonce: 3, balance: U256::from(4), code_hash: KECCAK_EMPTY }),
        };
        let encoded = BlockCodec::encode_account_changes(1, &[present, absent]).unwrap();
        assert_eq!(encoded.len(), 2 * ACCOUNT_CHANGE_LEN, "two fixed-width entries");
        assert_eq!(
            &encoded[20..ACCOUNT_CHANGE_LEN],
            &[0u8; ACCOUNT_CHANGE_LEN - 20][..],
            "absent sentinel is zeroed"
        );
        assert_eq!(encoded[ACCOUNT_CHANGE_LEN + 20], 1, "present flag");
        assert_eq!(
            BlockCodec::decode_account_changes(&encoded).unwrap(),
            vec![absent, present],
            "sorted by address"
        );
        assert!(
            BlockCodec::encode_account_changes(1, &[absent, absent]).is_err(),
            "duplicate account rejected"
        );
        let slot = StorageChange { address: Address::ZERO, slot: B256::ZERO, before: U256::ZERO };
        assert!(
            BlockCodec::encode_storage_changes(1, &[slot, slot]).is_err(),
            "duplicate slot rejected"
        );
        let mut bad = encoded;
        bad[ACCOUNT_CHANGE_LEN - 1] = 1;
        assert!(
            BlockCodec::decode_account_changes(&bad).is_err(),
            "non-zero absent entry rejected"
        );
    }
}
