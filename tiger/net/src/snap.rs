//! The `snap/2` wire protocol (EIP-8189) as base-reth serves it with `--snap.v2`, the store
//! interfaces snap sync reads and writes through, and the serving side.
//!
//! snap/2 keeps snap/1's account, storage and bytecode messages (ids 0x00 to 0x05) and replaces
//! trie-node healing (0x06/0x07) with block access lists (0x08/0x09). Base blocks carry no access
//! lists, so tiger answers GetBlockAccessLists with an empty list and heals with its own
//! executed-block diffs instead (see `snap_sync`).

use alloy_primitives::{B256, Bytes, KECCAK256_EMPTY, U256};
use alloy_rlp::{Decodable, Header as RlpHeader, RlpDecodable, RlpEncodable};
use alloy_trie::{EMPTY_ROOT_HASH, TrieAccount};

use crate::{
    eth::{SOFT_RESPONSE_LIMIT, encode_list, list_items},
    rlpx::LocalCapability,
};

/// The snap version tiger speaks, with its message id count (reth's `SnapVersion::V2`).
pub const SNAP_CAPABILITY: LocalCapability = LocalCapability { name: "snap", version: 2, count: 10 };

/// Relative snap message ids.
mod id {
    pub(crate) const GET_ACCOUNT_RANGE: u8 = 0x00;
    pub(crate) const ACCOUNT_RANGE: u8 = 0x01;
    pub(crate) const GET_STORAGE_RANGES: u8 = 0x02;
    pub(crate) const STORAGE_RANGES: u8 = 0x03;
    pub(crate) const GET_BYTE_CODES: u8 = 0x04;
    pub(crate) const BYTE_CODES: u8 = 0x05;
    pub(crate) const GET_BLOCK_ACCESS_LISTS: u8 = 0x08;
    pub(crate) const BLOCK_ACCESS_LISTS: u8 = 0x09;
}
pub(crate) use id::*;

/// Most bytecodes served per request (reth's `MAX_BYTE_CODES_SERVE`).
pub const MAX_BYTE_CODES_SERVE: usize = 1024;
/// Most accounts whose storage is served per request.
pub const MAX_STORAGE_ACCOUNTS_SERVE: usize = 1024;
/// The largest hashed key.
pub const MAX_HASH: B256 = B256::repeat_byte(0xff);

/// An error reading or writing the store, as reported by the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A read view of one state, for serving snap requests. Implemented by the store.
pub trait SnapStateView {
    /// Appends accounts with hashed address `>= start`, ascending, at most `max`. Each account
    /// carries its storage root in this state.
    fn accounts(
        &self,
        start: B256,
        max: usize,
        out: &mut Vec<(B256, TrieAccount)>,
    ) -> Result<(), StoreError>;

    /// Appends the nonzero slots of one account with hashed slot `>= start`, ascending, at most
    /// `max`.
    fn storage(
        &self,
        account: B256,
        start: B256,
        max: usize,
        out: &mut Vec<(B256, U256)>,
    ) -> Result<(), StoreError>;

    /// Appends the account trie nodes from the root along the path to `key`, as `eth_getProof`
    /// does; works for absent keys (a proof of absence).
    fn account_proof(&self, key: B256, out: &mut Vec<Bytes>) -> Result<(), StoreError>;

    /// Appends one account's storage trie nodes along the path to `key`.
    fn storage_proof(
        &self,
        account: B256,
        key: B256,
        out: &mut Vec<Bytes>,
    ) -> Result<(), StoreError>;
}

/// Opens state views by state root. Implemented by the store.
pub trait SnapStateSource: Send + Sync {
    /// A consistent view of the state with this root, if it is a retained recent canonical root
    /// (base-reth serves the last 128). `None` otherwise.
    fn open_state(&self, state_root: B256) -> Result<Option<Box<dyn SnapStateView + '_>>, StoreError>;

    /// The bytecode with this hash, if stored (code is content addressed, so no root is needed).
    fn code(&self, code_hash: B256) -> Result<Option<Bytes>, StoreError>;
}

/// An account as snap sync writes it into the hashed account table (no storage root: the store
/// derives storage roots from its tries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapAccount {
    /// The nonce.
    pub nonce: u64,
    /// The balance.
    pub balance: U256,
    /// The code hash (`KECCAK256_EMPTY` for no code).
    pub code_hash: B256,
}

/// A range of hashed keys whose leaves are complete in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapRange {
    /// `None` for the account trie, or the hashed address of a storage trie.
    pub trie: Option<B256>,
    /// The first key of the range.
    pub from: B256,
    /// The last key of the range, inclusive.
    pub to: B256,
}

/// One atomic snap write: verified state, the key ranges it completes, and the syncer's
/// progress. The store commits all of it in one transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapBatch {
    /// Hashed account upserts.
    pub accounts: Vec<(B256, SnapAccount)>,
    /// Hashed storage upserts: (hashed address, hashed slot, nonzero value).
    pub storage: Vec<(B256, B256, U256)>,
    /// Bytecodes by hash.
    pub codes: Vec<(B256, Bytes)>,
    /// Ranges whose leaves are complete once this batch commits. An account range marks only
    /// the account leaves; each storage trie is complete once its own range is sent.
    pub completed: Vec<SnapRange>,
    /// The syncer's opaque progress, returned by [`SnapStateSink::load_progress`] after restart.
    pub progress: Vec<u8>,
}

/// Where snap sync writes. Implemented by the store (through the node's adapter).
pub trait SnapStateSink: Send {
    /// The progress from the last successful commit, `None` on a fresh start.
    fn load_progress(&self) -> Result<Option<Vec<u8>>, StoreError>;
    /// Commits a batch atomically.
    fn commit_snap(&mut self, batch: &SnapBatch) -> Result<(), StoreError>;
    /// True if the code is stored already.
    fn has_code(&self, code_hash: B256) -> Result<bool, StoreError>;
    /// Called once every range is complete: rebuild the tries from the hashed tables, check the
    /// root against the head header, and mark the whole state complete.
    fn finish_snap(&mut self, head: u64, expected_root: B256) -> Result<(), StoreError>;
}

/// Encodes an account in snap's slim form: empty strings for the empty storage root and code.
pub fn encode_slim_account(account: &TrieAccount, out: &mut Vec<u8>) {
    let storage_root: &[u8] =
        if account.storage_root == EMPTY_ROOT_HASH { &[] } else { account.storage_root.as_slice() };
    let code_hash: &[u8] =
        if account.code_hash == KECCAK256_EMPTY { &[] } else { account.code_hash.as_slice() };
    encode_list(&[&account.nonce, &account.balance, &storage_root, &code_hash], out);
}

fn restore_hash(bytes: &[u8], empty: B256) -> alloy_rlp::Result<B256> {
    match bytes.len() {
        0 => Ok(empty),
        32 => Ok(B256::from_slice(bytes)),
        _ => Err(alloy_rlp::Error::UnexpectedLength),
    }
}

/// Decodes a slim account.
pub fn decode_slim_account(buf: &mut &[u8]) -> alloy_rlp::Result<TrieAccount> {
    let mut items = list_items(buf)?;
    let nonce = u64::decode(&mut items)?;
    let balance = U256::decode(&mut items)?;
    let storage_root = restore_hash(RlpHeader::decode_bytes(&mut items, false)?, EMPTY_ROOT_HASH)?;
    let code_hash = restore_hash(RlpHeader::decode_bytes(&mut items, false)?, KECCAK256_EMPTY)?;
    Ok(TrieAccount { nonce, balance, storage_root, code_hash })
}

/// A GetAccountRange request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct GetAccountRange {
    /// Request id.
    pub request_id: u64,
    /// The state root to serve from.
    pub root: B256,
    /// First hashed address.
    pub start: B256,
    /// Last hashed address wanted (the response may stop earlier, or include one past it).
    pub limit: B256,
    /// Soft response size limit.
    pub response_bytes: u64,
}

/// An AccountRange response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRange {
    /// Request id.
    pub request_id: u64,
    /// Consecutive accounts by hashed address.
    pub accounts: Vec<(B256, TrieAccount)>,
    /// Boundary proof nodes.
    pub proof: Vec<Bytes>,
}

impl AccountRange {
    /// Encodes the response.
    pub fn encode_payload(&self) -> Vec<u8> {
        let mut accounts = Vec::new();
        for (hash, account) in &self.accounts {
            let mut body = Vec::new();
            encode_slim_account(account, &mut body);
            encode_list(&[hash, &crate::eth::RawRlp(&body)], &mut accounts);
        }
        let mut list = Vec::new();
        RlpHeader { list: true, payload_length: accounts.len() }.encode(&mut list);
        list.extend_from_slice(&accounts);
        let mut out = Vec::new();
        encode_list(&[&self.request_id, &crate::eth::RawRlp(&list), &self.proof], &mut out);
        out
    }

    /// Decodes the response.
    pub fn decode_payload(payload: &[u8]) -> alloy_rlp::Result<Self> {
        let mut buf = payload;
        let mut items = list_items(&mut buf)?;
        let request_id = u64::decode(&mut items)?;
        let mut list = list_items(&mut items)?;
        let mut accounts = Vec::new();
        while !list.is_empty() {
            let mut entry = list_items(&mut list)?;
            let hash = B256::decode(&mut entry)?;
            accounts.push((hash, decode_slim_account(&mut entry)?));
        }
        let proof = Vec::<Bytes>::decode(&mut items)?;
        Ok(Self { request_id, accounts, proof })
    }
}

/// A storage range bound: a hash, or the empty string for "unbounded" (0x00.. or 0xff..).
fn decode_bound(buf: &mut &[u8], default: B256) -> alloy_rlp::Result<B256> {
    restore_hash(RlpHeader::decode_bytes(buf, false)?, default)
}

/// A GetStorageRanges request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetStorageRanges {
    /// Request id.
    pub request_id: u64,
    /// The state root to serve from.
    pub root: B256,
    /// Hashed addresses whose storage is requested.
    pub accounts: Vec<B256>,
    /// First hashed slot (applies to the first account only).
    pub start: B256,
    /// Last hashed slot wanted (applies to the last account only).
    pub limit: B256,
    /// Soft response size limit.
    pub response_bytes: u64,
}

impl GetStorageRanges {
    /// Encodes the request (bounds always as full hashes).
    pub fn encode_payload(&self) -> Vec<u8> {
        let mut out = Vec::new();
        encode_list(
            &[
                &self.request_id,
                &self.root,
                &self.accounts,
                &self.start,
                &self.limit,
                &self.response_bytes,
            ],
            &mut out,
        );
        out
    }

    /// Decodes the request; empty bounds mean 0x00.. and 0xff...
    pub fn decode_payload(payload: &[u8]) -> alloy_rlp::Result<Self> {
        let mut buf = payload;
        let mut items = list_items(&mut buf)?;
        Ok(Self {
            request_id: u64::decode(&mut items)?,
            root: B256::decode(&mut items)?,
            accounts: Vec::<B256>::decode(&mut items)?,
            start: decode_bound(&mut items, B256::ZERO)?,
            limit: decode_bound(&mut items, MAX_HASH)?,
            response_bytes: u64::decode(&mut items)?,
        })
    }
}

/// A StorageRanges response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRanges {
    /// Request id.
    pub request_id: u64,
    /// For each served account, in request order, its consecutive (hashed slot, value) pairs.
    pub slots: Vec<Vec<(B256, U256)>>,
    /// Boundary proof for the last served account, if its range is partial.
    pub proof: Vec<Bytes>,
}

impl StorageRanges {
    /// Encodes the response; slot values travel RLP-encoded, as in the trie.
    pub fn encode_payload(&self) -> Vec<u8> {
        let mut lists = Vec::new();
        for slots in &self.slots {
            let mut entries = Vec::new();
            for (hash, value) in slots {
                let data = alloy_rlp::encode(value);
                encode_list(&[hash, &data.as_slice()], &mut entries);
            }
            RlpHeader { list: true, payload_length: entries.len() }.encode(&mut lists);
            lists.extend_from_slice(&entries);
        }
        let mut wrapped = Vec::new();
        RlpHeader { list: true, payload_length: lists.len() }.encode(&mut wrapped);
        wrapped.extend_from_slice(&lists);
        let mut out = Vec::new();
        encode_list(&[&self.request_id, &crate::eth::RawRlp(&wrapped), &self.proof], &mut out);
        out
    }

    /// Decodes the response.
    pub fn decode_payload(payload: &[u8]) -> alloy_rlp::Result<Self> {
        let mut buf = payload;
        let mut items = list_items(&mut buf)?;
        let request_id = u64::decode(&mut items)?;
        let mut lists = list_items(&mut items)?;
        let mut slots = Vec::new();
        while !lists.is_empty() {
            let mut entries = list_items(&mut lists)?;
            let mut account = Vec::new();
            while !entries.is_empty() {
                let mut entry = list_items(&mut entries)?;
                let hash = B256::decode(&mut entry)?;
                let mut data = RlpHeader::decode_bytes(&mut entry, false)?;
                let value = U256::decode(&mut data)?;
                if !data.is_empty() || value.is_zero() {
                    return Err(alloy_rlp::Error::Custom("bad storage slot value"));
                }
                account.push((hash, value));
            }
            slots.push(account);
        }
        let proof = Vec::<Bytes>::decode(&mut items)?;
        Ok(Self { request_id, slots, proof })
    }
}

/// A GetByteCodes request.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct GetByteCodes {
    /// Request id.
    pub request_id: u64,
    /// Code hashes.
    pub hashes: Vec<B256>,
    /// Soft response size limit.
    pub response_bytes: u64,
}

/// A ByteCodes response.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct ByteCodes {
    /// Request id.
    pub request_id: u64,
    /// Codes, in request order, possibly with gaps for unknown hashes.
    pub codes: Vec<Bytes>,
}

/// Serves snap requests from a [`SnapStateSource`]. Returns the relative response id and the
/// encoded payload, or `None` for a message that is not a snap request.
pub fn serve(source: &dyn SnapStateSource, message_id: u8, payload: &[u8]) -> Option<(u8, Vec<u8>)> {
    match message_id {
        GET_ACCOUNT_RANGE => {
            let request = GetAccountRange::decode(&mut &payload[..]).ok()?;
            let response = serve_account_range(source, &request).unwrap_or(AccountRange {
                request_id: request.request_id,
                accounts: Vec::new(),
                proof: Vec::new(),
            });
            Some((ACCOUNT_RANGE, response.encode_payload()))
        }
        GET_STORAGE_RANGES => {
            let request = GetStorageRanges::decode_payload(payload).ok()?;
            let response = serve_storage_ranges(source, &request).unwrap_or(StorageRanges {
                request_id: request.request_id,
                slots: Vec::new(),
                proof: Vec::new(),
            });
            Some((STORAGE_RANGES, response.encode_payload()))
        }
        GET_BYTE_CODES => {
            let request = GetByteCodes::decode(&mut &payload[..]).ok()?;
            let codes = serve_byte_codes(source, &request).unwrap_or_default();
            Some((BYTE_CODES, alloy_rlp::encode(ByteCodes { request_id: request.request_id, codes })))
        }
        GET_BLOCK_ACCESS_LISTS => {
            // Base blocks have no access lists: answer `[request_id, []]`.
            let (request_id, _) = crate::eth::split_request_id(payload).ok()?;
            Some((BLOCK_ACCESS_LISTS, crate::eth::encode_response::<&[u8]>(request_id, &[])))
        }
        _ => None,
    }
}

fn response_limit(requested: u64) -> usize {
    (requested as usize).min(SOFT_RESPONSE_LIMIT)
}

/// Rough wire size of one account entry, used against the response byte limit.
const ACCOUNT_ENTRY_BYTES: usize = 32 + 4 * 32;

fn serve_account_range(
    source: &dyn SnapStateSource,
    request: &GetAccountRange,
) -> Result<AccountRange, StoreError> {
    let empty = AccountRange { request_id: request.request_id, accounts: Vec::new(), proof: Vec::new() };
    let Some(view) = source.open_state(request.root)? else { return Ok(empty) };
    if request.start > request.limit {
        return Ok(empty);
    }
    let max = (response_limit(request.response_bytes) / ACCOUNT_ENTRY_BYTES).max(1);
    let mut accounts = Vec::new();
    view.accounts(request.start, max, &mut accounts)?;
    let exhausted = accounts.len() < max;
    // Keep accounts up to the limit, plus the first one past it, which proves the gap.
    let mut truncated = false;
    if let Some(past) = accounts.iter().position(|(hash, _)| *hash > request.limit) {
        truncated = past + 1 < accounts.len();
        accounts.truncate(past + 1);
    }
    // A zero-origin range that reached the end of the trie needs no proof: the leaves alone
    // rebuild the root. Otherwise prove the origin and the last returned key.
    let exhausted = exhausted && !truncated;
    let mut proof = Vec::new();
    if !(request.start.is_zero() && exhausted) {
        view.account_proof(request.start, &mut proof)?;
        if let Some((last, _)) = accounts.last() {
            view.account_proof(*last, &mut proof)?;
        }
        dedup_proof(&mut proof);
    }
    Ok(AccountRange { request_id: request.request_id, accounts, proof })
}

fn serve_storage_ranges(
    source: &dyn SnapStateSource,
    request: &GetStorageRanges,
) -> Result<StorageRanges, StoreError> {
    let mut response =
        StorageRanges { request_id: request.request_id, slots: Vec::new(), proof: Vec::new() };
    let Some(view) = source.open_state(request.root)? else { return Ok(response) };
    let limit = response_limit(request.response_bytes);
    let mut bytes = 0usize;
    let count = request.accounts.len().min(MAX_STORAGE_ACCOUNTS_SERVE);
    for (index, account) in request.accounts[..count].iter().enumerate() {
        let start = if index == 0 { request.start } else { B256::ZERO };
        let end = if index + 1 == count { request.limit } else { MAX_HASH };
        let budget = ((limit.saturating_sub(bytes)) / 64).max(1);
        let mut slots = Vec::new();
        view.storage(*account, start, budget, &mut slots)?;
        let mut partial = slots.len() >= budget || !start.is_zero();
        if let Some(past) = slots.iter().position(|(hash, _)| *hash > end) {
            partial |= past + 1 < slots.len();
            slots.truncate(past + 1);
        }
        bytes += slots.len() * 64;
        if partial {
            view.storage_proof(*account, start, &mut response.proof)?;
            if let Some((last, _)) = slots.last() {
                view.storage_proof(*account, *last, &mut response.proof)?;
            }
            dedup_proof(&mut response.proof);
            response.slots.push(slots);
            break;
        }
        response.slots.push(slots);
        if bytes >= limit {
            break;
        }
    }
    Ok(response)
}

fn serve_byte_codes(
    source: &dyn SnapStateSource,
    request: &GetByteCodes,
) -> Result<Vec<Bytes>, StoreError> {
    let limit = response_limit(request.response_bytes);
    let mut codes = Vec::new();
    let mut bytes = 0;
    for hash in request.hashes.iter().take(MAX_BYTE_CODES_SERVE) {
        let code = if *hash == KECCAK256_EMPTY {
            Bytes::new()
        } else {
            match source.code(*hash)? {
                Some(code) => code,
                None => continue,
            }
        };
        bytes += code.len();
        codes.push(code);
        if bytes > limit {
            break;
        }
    }
    Ok(codes)
}

fn dedup_proof(proof: &mut Vec<Bytes>) {
    let mut seen = std::collections::HashSet::new();
    proof.retain(|node| seen.insert(alloy_primitives::keccak256(node)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let account = TrieAccount {
            nonce: 3,
            balance: U256::from(10),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: B256::repeat_byte(5),
        };
        let range = AccountRange {
            request_id: 7,
            accounts: vec![(B256::repeat_byte(1), account), (B256::repeat_byte(2), TrieAccount::default())],
            proof: vec![Bytes::from_static(b"\xc2\x01\x02")],
        };
        assert_eq!(AccountRange::decode_payload(&range.encode_payload()).unwrap(), range);

        let storage = StorageRanges {
            request_id: 8,
            slots: vec![vec![(B256::repeat_byte(3), U256::from(1))], vec![]],
            proof: vec![],
        };
        assert_eq!(StorageRanges::decode_payload(&storage.encode_payload()).unwrap(), storage);

        let request = GetStorageRanges {
            request_id: 9,
            root: B256::repeat_byte(4),
            accounts: vec![B256::repeat_byte(6)],
            start: B256::ZERO,
            limit: MAX_HASH,
            response_bytes: 500_000,
        };
        assert_eq!(GetStorageRanges::decode_payload(&request.encode_payload()).unwrap(), request);
    }

    #[test]
    fn slim_account_uses_empty_strings_for_defaults() {
        let mut out = Vec::new();
        encode_slim_account(&TrieAccount::default(), &mut out);
        // [nonce 0, balance 0, "", ""] -- every field is an empty string.
        assert_eq!(out, vec![0xc4, 0x80, 0x80, 0x80, 0x80]);
        assert_eq!(decode_slim_account(&mut out.as_slice()).unwrap(), TrieAccount::default());
    }
}
