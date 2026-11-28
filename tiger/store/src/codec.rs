//! reth's `Compact` value encodings and table key encodings, written by hand.
//!
//! Each type matches the bytes reth's `reth-codecs` (0.8.2) derive or hand-written impl produces
//! at base/reth 2b20e42; the tests check fixed vectors, and the datadir tests in this crate
//! round-trip every entry of a datadir base-reth wrote.
//!
//! The derive's scheme for a struct: a bit-packed flags header (modular-bitfield layout: fields
//! in declaration order from the least significant bit of the first byte, padded to whole
//! bytes), where `u64` fields store their byte length in 4 bits, `U256` in 6 bits, `u128` in 5,
//! and `bool`/`Option` in 1 bit; then each field's bytes in order. Integers drop leading zero
//! bytes. An `Option<B256>`/`Option<Address>` field holds the raw bytes; any other `Option<T>`
//! holds a varuint length and `T`'s bytes.

use alloy_primitives::{Address, B256, TxKind, U256};
use alloy_trie::{BranchNodeCompact, Nibbles, TrieMask};

/// A value that does not decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError(pub &'static str);

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "undecodable value: {}", self.0)
    }
}

impl std::error::Error for CodecError {}

/// Decoding result.
pub type Decoded<T> = Result<T, CodecError>;

/// A cursor over bytes being decoded.
#[derive(Debug, Clone, Copy)]
pub struct Reader<'a> {
    /// The bytes not read yet.
    pub rest: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Starts reading `bytes`.
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    /// Takes `n` bytes.
    pub fn take(&mut self, n: usize) -> Decoded<&'a [u8]> {
        if self.rest.len() < n {
            return Err(CodecError("truncated"));
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    /// Takes one byte.
    pub fn u8(&mut self) -> Decoded<u8> {
        Ok(self.take(1)?[0])
    }

    /// Takes a big-endian `u16`.
    pub fn u16(&mut self) -> Decoded<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    /// Takes a big-endian `u32`.
    pub fn u32(&mut self) -> Decoded<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    /// Takes a big-endian `u64`.
    pub fn u64_be(&mut self) -> Decoded<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// Takes a 32-byte hash.
    pub fn b256(&mut self) -> Decoded<B256> {
        Ok(B256::from_slice(self.take(32)?))
    }

    /// Takes a 20-byte address.
    pub fn address(&mut self) -> Decoded<Address> {
        Ok(Address::from_slice(self.take(20)?))
    }

    /// Takes a compact (leading-zero-stripped) integer of `len` bytes.
    pub fn compact_u64(&mut self, len: usize) -> Decoded<u64> {
        if len > 8 {
            return Err(CodecError("u64 longer than 8 bytes"));
        }
        let mut out = [0u8; 8];
        out[8 - len..].copy_from_slice(self.take(len)?);
        Ok(u64::from_be_bytes(out))
    }

    /// Takes a compact `u128` of `len` bytes.
    pub fn compact_u128(&mut self, len: usize) -> Decoded<u128> {
        if len > 16 {
            return Err(CodecError("u128 longer than 16 bytes"));
        }
        let mut out = [0u8; 16];
        out[16 - len..].copy_from_slice(self.take(len)?);
        Ok(u128::from_be_bytes(out))
    }

    /// Takes a compact `U256` of `len` bytes.
    pub fn compact_u256(&mut self, len: usize) -> Decoded<U256> {
        if len > 32 {
            return Err(CodecError("U256 longer than 32 bytes"));
        }
        Ok(U256::from_be_slice(self.take(len)?))
    }

    /// Takes a varuint (7 bits per byte, least significant group first).
    pub fn varuint(&mut self) -> Decoded<usize> {
        let mut value = 0usize;
        for i in 0..10 {
            let byte = self.u8()?;
            value |= usize::from(byte & 0x7f) << (i * 7);
            if byte < 0x80 {
                return Ok(value);
            }
        }
        Err(CodecError("varuint too long"))
    }

    /// Takes a varuint-length-prefixed byte string.
    pub fn bytes(&mut self) -> Decoded<&'a [u8]> {
        let len = self.varuint()?;
        self.take(len)
    }

    /// Takes the derive's flags header of `n` bytes.
    pub fn flags(&mut self, n: usize) -> Decoded<Flags> {
        let mut bits = 0u64;
        for (i, byte) in self.take(n)?.iter().enumerate() {
            bits |= u64::from(*byte) << (8 * i);
        }
        Ok(Flags { bits, used: 0 })
    }

    /// Fails unless every byte was read.
    pub fn finish(self) -> Decoded<()> {
        if self.rest.is_empty() { Ok(()) } else { Err(CodecError("trailing bytes")) }
    }
}

/// The derive's bit-packed flags header, read or written field by field.
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    /// The bits, first field in the least significant bits.
    pub bits: u64,
    /// Bits consumed or produced so far.
    pub used: u32,
}

impl Flags {
    /// Appends a field of `width` bits.
    pub fn put(&mut self, width: u32, value: u64) {
        assert!(width <= 8 && value < (1 << width), "flag field overflow");
        self.bits |= value << self.used;
        self.used += width;
        assert!(self.used <= 64);
    }

    /// Reads the next field of `width` bits.
    pub fn get(&mut self, width: u32) -> u64 {
        let value = (self.bits >> self.used) & ((1 << width) - 1);
        self.used += width;
        value
    }

    /// The header bytes (bits padded to whole bytes).
    pub fn write(&self, out: &mut Vec<u8>) {
        let bytes = self.used.div_ceil(8) as usize;
        out.extend_from_slice(&self.bits.to_le_bytes()[..bytes]);
    }
}

/// Appends a compact integer and returns its length.
pub fn put_compact_u64(out: &mut Vec<u8>, value: u64) -> u64 {
    let skip = value.leading_zeros() as usize / 8;
    out.extend_from_slice(&value.to_be_bytes()[skip..]);
    (8 - skip) as u64
}

/// Appends a compact `u128` and returns its length.
pub fn put_compact_u128(out: &mut Vec<u8>, value: u128) -> u64 {
    let skip = value.leading_zeros() as usize / 8;
    out.extend_from_slice(&value.to_be_bytes()[skip..]);
    (16 - skip) as u64
}

/// Appends a compact `U256` and returns its length.
pub fn put_compact_u256(out: &mut Vec<u8>, value: U256) -> u64 {
    let skip = value.leading_zeros() / 8;
    out.extend_from_slice(&value.to_be_bytes::<32>()[skip..]);
    (32 - skip) as u64
}

/// Appends a varuint.
pub fn put_varuint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// A `Compact` struct encoder: the flags header and the field bytes, written field by field.
#[derive(Default)]
pub(crate) struct W {
    pub(crate) flags: Flags,
    pub(crate) body: Vec<u8>,
}

impl W {
    pub(crate) fn u64(&mut self, v: u64) {
        let len = put_compact_u64(&mut self.body, v);
        self.flags.put(4, len);
    }
    pub(crate) fn u128(&mut self, v: u128) {
        let len = put_compact_u128(&mut self.body, v);
        self.flags.put(5, len);
    }
    pub(crate) fn u256(&mut self, v: U256) {
        let len = put_compact_u256(&mut self.body, v);
        self.flags.put(6, len);
    }
    pub(crate) fn bool(&mut self, v: bool) {
        self.flags.put(1, u64::from(v));
    }
    pub(crate) fn kind(&mut self, v: TxKind) {
        self.opt_raw(match &v {
            TxKind::Create => None,
            TxKind::Call(a) => Some(a.as_slice()),
        });
    }
    /// `Option<u64>` / `Option<u128>`: a presence bit, then length-prefixed compact bytes.
    pub(crate) fn opt(&mut self, v: Option<u128>) {
        self.flags.put(1, u64::from(v.is_some()));
        if let Some(v) = v {
            let mut tmp = Vec::with_capacity(16);
            put_compact_u128(&mut tmp, v);
            put_varuint(&mut self.body, tmp.len());
            self.body.extend_from_slice(&tmp);
        }
    }
    /// `Option<Address>` / `Option<B256>`: a presence bit, then the raw bytes.
    pub(crate) fn opt_raw(&mut self, v: Option<&[u8]>) {
        self.flags.put(1, u64::from(v.is_some()));
        if let Some(v) = v {
            self.body.extend_from_slice(v);
        }
    }
    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        self.body.extend_from_slice(bytes);
    }
    /// Appends the flags header, then the fields.
    pub(crate) fn write(self, out: &mut Vec<u8>) {
        self.flags.write(out);
        out.extend_from_slice(&self.body);
    }
    pub(crate) fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 4);
        self.write(&mut out);
        out
    }
}

/// A `Compact` struct decoder: the flags header read field by field over the field bytes.
pub(crate) struct F<'a, 'b> {
    pub(crate) r: &'b mut Reader<'a>,
    pub(crate) flags: Flags,
}

impl F<'_, '_> {
    pub(crate) fn u64(&mut self) -> Decoded<u64> {
        let len = self.flags.get(4) as usize;
        self.r.compact_u64(len)
    }
    pub(crate) fn u128(&mut self) -> Decoded<u128> {
        let len = self.flags.get(5) as usize;
        self.r.compact_u128(len)
    }
    pub(crate) fn u256(&mut self) -> Decoded<U256> {
        let len = self.flags.get(6) as usize;
        self.r.compact_u256(len)
    }
    pub(crate) fn bool(&mut self) -> bool {
        self.flags.get(1) == 1
    }
    pub(crate) fn kind(&mut self) -> Decoded<TxKind> {
        Ok(self.opt_address()?.map_or(TxKind::Create, TxKind::Call))
    }
    pub(crate) fn opt_address(&mut self) -> Decoded<Option<Address>> {
        if self.bool() { Ok(Some(self.r.address()?)) } else { Ok(None) }
    }
    pub(crate) fn opt_b256(&mut self) -> Decoded<Option<B256>> {
        if self.bool() { Ok(Some(self.r.b256()?)) } else { Ok(None) }
    }
    /// `Option<u128>`: a presence bit, then length-prefixed compact bytes.
    pub(crate) fn opt(&mut self) -> Decoded<Option<u128>> {
        if !self.bool() {
            return Ok(None);
        }
        let len = self.r.varuint()?;
        Ok(Some(self.r.compact_u128(len)?))
    }
    /// `Option<u64>`, which rejects more than 8 bytes.
    pub(crate) fn opt_u64(&mut self) -> Decoded<Option<u64>> {
        if !self.bool() {
            return Ok(None);
        }
        let len = self.r.varuint()?;
        Ok(Some(self.r.compact_u64(len)?))
    }
}

/// An account as reth stores it (`PlainAccountState`, `HashedAccounts`, changesets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Account {
    /// Nonce.
    pub nonce: u64,
    /// Balance.
    pub balance: U256,
    /// Code hash, `None` for the empty code.
    pub bytecode_hash: Option<B256>,
}

impl Account {
    /// The `Compact` encoding: flags (nonce 4 bits, balance 6, code hash 1; 2 bytes), nonce,
    /// balance, raw code hash.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut w = W { flags: Flags::default(), body: Vec::with_capacity(72) };
        w.u64(self.nonce);
        w.u256(self.balance);
        w.opt_raw(self.bytecode_hash.as_ref().map(|h| h.as_slice()));
        w.write(out);
    }

    /// Encodes into a new buffer.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(74);
        self.encode(&mut out);
        out
    }

    /// Reads an account.
    pub fn decode(reader: &mut Reader<'_>) -> Decoded<Self> {
        let mut f = F { flags: reader.flags(2)?, r: reader };
        Ok(Self { nonce: f.u64()?, balance: f.u256()?, bytecode_hash: f.opt_b256()? })
    }

    /// Decodes a whole value.
    pub fn from_bytes(bytes: &[u8]) -> Decoded<Self> {
        let mut reader = Reader::new(bytes);
        let account = Self::decode(&mut reader)?;
        reader.finish()?;
        Ok(account)
    }
}

/// A storage slot as reth stores it in `DUPSORT` storage tables: the slot key (the sub-key)
/// followed by the compact value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StorageEntry {
    /// The slot (plain or hashed, by table).
    pub key: B256,
    /// The value.
    pub value: U256,
}

impl StorageEntry {
    /// The encoding: 32-byte key, compact value.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(self.key.as_slice());
        put_compact_u256(&mut out, self.value);
        out
    }

    /// Decodes a whole value.
    pub fn from_bytes(bytes: &[u8]) -> Decoded<Self> {
        let mut reader = Reader::new(bytes);
        let key = reader.b256()?;
        let value = reader.compact_u256(reader.rest.len())?;
        Ok(Self { key, value })
    }
}

/// An account changeset entry: the address and the account before the block (`None` when it did
/// not exist).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountBeforeTx {
    /// The address.
    pub address: Address,
    /// The account before the change.
    pub info: Option<Account>,
}

impl AccountBeforeTx {
    /// The encoding: 20-byte address, then the account if any.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(94);
        out.extend_from_slice(self.address.as_slice());
        if let Some(account) = &self.info {
            account.encode(&mut out);
        }
        out
    }

    /// Decodes a whole value.
    pub fn from_bytes(bytes: &[u8]) -> Decoded<Self> {
        let mut reader = Reader::new(bytes);
        let address = reader.address()?;
        let info = if reader.rest.is_empty() { None } else { Some(Account::decode(&mut reader)?) };
        reader.finish()?;
        Ok(Self { address, info })
    }
}

/// A storage changeset entry (`StorageChangeSets` static file rows): the plain address and slot
/// and the value before the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageBeforeTx {
    /// The address.
    pub address: Address,
    /// The plain slot.
    pub slot: B256,
    /// The value before the change (zero when unset).
    pub value: U256,
}

impl StorageBeforeTx {
    /// The encoding: 20-byte address, 32-byte slot, compact value.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(84);
        out.extend_from_slice(self.address.as_slice());
        out.extend_from_slice(self.slot.as_slice());
        put_compact_u256(&mut out, self.value);
        out
    }

    /// Decodes a whole row.
    pub fn from_bytes(bytes: &[u8]) -> Decoded<Self> {
        let mut reader = Reader::new(bytes);
        let address = reader.address()?;
        let slot = reader.b256()?;
        let value = reader.compact_u256(reader.rest.len())?;
        Ok(Self { address, slot, value })
    }
}

/// reth's `StageCheckpoint` value: flags (block number length 4 bits, stage progress present 1
/// bit), then the block number. Tiger writes no stage progress and ignores it on read.
#[derive(Debug)]
pub struct StageCheckpointCodec;

impl StageCheckpointCodec {
    /// Encodes a checkpoint at `block`.
    pub fn encode(block: u64) -> Vec<u8> {
        let mut w = W::default();
        w.u64(block);
        w.bool(false);
        w.finish()
    }

    /// The block number of a stored checkpoint.
    pub fn decode(bytes: &[u8]) -> Decoded<u64> {
        let mut reader = Reader::new(bytes);
        F { flags: reader.flags(1)?, r: &mut reader }.u64()
    }
}

/// reth's `StoredBlockBodyIndices`: the first transaction number and the transaction count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockBodyIndices {
    /// The block's first transaction number.
    pub first_tx: u64,
    /// The block's transaction count.
    pub tx_count: u64,
}

impl BlockBodyIndices {
    /// Flags (4 bits per field, 1 byte), then both compact integers.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = W::default();
        w.u64(self.first_tx);
        w.u64(self.tx_count);
        w.finish()
    }

    /// Decodes a whole value.
    pub fn from_bytes(bytes: &[u8]) -> Decoded<Self> {
        let mut reader = Reader::new(bytes);
        let mut f = F { flags: reader.flags(1)?, r: &mut reader };
        let indices = Self { first_tx: f.u64()?, tx_count: f.u64()? };
        reader.finish()?;
        Ok(indices)
    }

    /// The first transaction number after the block.
    pub const fn next_tx(&self) -> u64 {
        self.first_tx + self.tx_count
    }
}

/// reth's `Compact` encoding of a block header (reth-codecs `alloy/header.rs`): a 4-byte flags
/// header, the fixed-size fields raw, the integers compact, optional hashes raw, optional
/// integers length-prefixed, the extension fields length-prefixed, and the extra data last.
#[derive(Debug)]
pub struct HeaderCodec;

impl HeaderCodec {
    /// Encodes `header`.
    pub fn encode(header: &alloy_consensus::Header) -> Vec<u8> {
        let mut w = W { flags: Flags::default(), body: Vec::with_capacity(600) };
        for hash in [header.parent_hash, header.ommers_hash] {
            w.raw(hash.as_slice());
        }
        w.raw(header.beneficiary.as_slice());
        for hash in [header.state_root, header.transactions_root, header.receipts_root] {
            w.raw(hash.as_slice());
        }
        w.opt_raw(header.withdrawals_root.as_ref().map(|h| h.as_slice()));
        w.raw(header.logs_bloom.as_slice());
        w.u256(header.difficulty);
        for value in [header.number, header.gas_limit, header.gas_used, header.timestamp] {
            w.u64(value);
        }
        w.raw(header.mix_hash.as_slice());
        w.u64(u64::from_be_bytes(header.nonce.0));
        for value in [header.base_fee_per_gas, header.blob_gas_used, header.excess_blob_gas] {
            w.opt(value.map(u128::from));
        }
        w.opt_raw(header.parent_beacon_block_root.as_ref().map(|h| h.as_slice()));
        let has_ext = header.requests_hash.is_some() || header.block_access_list_hash.is_some() || header.slot_number.is_some();
        w.bool(has_ext);
        if has_ext {
            let mut ext = W::default();
            ext.opt_raw(header.requests_hash.as_ref().map(|h| h.as_slice()));
            ext.opt_raw(header.block_access_list_hash.as_ref().map(|h| h.as_slice()));
            ext.opt(header.slot_number.map(u128::from));
            let encoded = ext.finish();
            put_varuint(&mut w.body, encoded.len());
            w.raw(&encoded);
        }
        w.raw(&header.extra_data);
        assert_eq!(w.flags.used, 32);
        w.finish()
    }

    /// Decodes a whole value.
    pub fn decode(bytes: &[u8]) -> Decoded<alloy_consensus::Header> {
        let mut r = Reader::new(bytes);
        let mut f = F { flags: r.flags(4)?, r: &mut r };
        let parent_hash = f.r.b256()?;
        let ommers_hash = f.r.b256()?;
        let beneficiary = f.r.address()?;
        let state_root = f.r.b256()?;
        let transactions_root = f.r.b256()?;
        let receipts_root = f.r.b256()?;
        let withdrawals_root = f.opt_b256()?;
        let logs_bloom = alloy_primitives::Bloom::from_slice(f.r.take(256)?);
        let difficulty = f.u256()?;
        let number = f.u64()?;
        let gas_limit = f.u64()?;
        let gas_used = f.u64()?;
        let timestamp = f.u64()?;
        let mix_hash = f.r.b256()?;
        let nonce = f.u64()?;
        let base_fee_per_gas = f.opt_u64()?;
        let blob_gas_used = f.opt_u64()?;
        let excess_blob_gas = f.opt_u64()?;
        let parent_beacon_block_root = f.opt_b256()?;
        let (mut requests_hash, mut block_access_list_hash, mut slot_number) = (None, None, None);
        if f.bool() {
            let len = f.r.varuint()?;
            let mut e = Reader::new(f.r.take(len)?);
            let mut ef = F { flags: e.flags(1)?, r: &mut e };
            requests_hash = ef.opt_b256()?;
            block_access_list_hash = ef.opt_b256()?;
            slot_number = ef.opt_u64()?;
            e.finish()?;
        }
        let extra_data = alloy_primitives::Bytes::copy_from_slice(r.rest);
        Ok(alloy_consensus::Header {
            parent_hash,
            ommers_hash,
            beneficiary,
            state_root,
            transactions_root,
            receipts_root,
            logs_bloom,
            difficulty,
            number,
            gas_limit,
            gas_used,
            timestamp,
            extra_data,
            mix_hash,
            nonce: nonce.to_be_bytes().into(),
            base_fee_per_gas,
            withdrawals_root,
            blob_gas_used,
            excess_blob_gas,
            parent_beacon_block_root,
            requests_hash,
            block_access_list_hash,
            slot_number,
        })
    }
}

/// Trie node encodings: `BranchNodeCompact` values and the packed nibble keys of storage v2.
#[derive(Debug)]
pub struct TrieCodec;

impl TrieCodec {
    /// `BranchNodeCompact`: state, tree and hash masks (big-endian `u16`s), the root hash when
    /// present, then the child hashes.
    pub fn encode_node(node: &BranchNodeCompact, out: &mut Vec<u8>) {
        out.extend_from_slice(&node.state_mask.get().to_be_bytes());
        out.extend_from_slice(&node.tree_mask.get().to_be_bytes());
        out.extend_from_slice(&node.hash_mask.get().to_be_bytes());
        if let Some(root) = node.root_hash {
            out.extend_from_slice(root.as_slice());
        }
        for hash in node.hashes.iter() {
            out.extend_from_slice(hash.as_slice());
        }
    }

    /// Decodes a `BranchNodeCompact`. The root hash is present exactly when there is one more
    /// hash than the hash mask has bits.
    pub fn decode_node(bytes: &[u8]) -> Decoded<BranchNodeCompact> {
        if bytes.len() < 6 || (bytes.len() - 6) % 32 != 0 {
            return Err(CodecError("branch node length"));
        }
        let mut reader = Reader::new(bytes);
        let state_mask = TrieMask::new(reader.u16()?);
        let tree_mask = TrieMask::new(reader.u16()?);
        let hash_mask = TrieMask::new(reader.u16()?);
        let mut count = reader.rest.len() / 32;
        let root_hash = if hash_mask.count_ones() as usize + 1 == count {
            count -= 1;
            Some(reader.b256()?)
        } else {
            None
        };
        if count != hash_mask.count_ones() as usize {
            return Err(CodecError("branch node hash count"));
        }
        let hashes = (0..count).map(|_| reader.b256()).collect::<Decoded<Vec<_>>>()?;
        if !tree_mask.is_subset_of(state_mask) || !hash_mask.is_subset_of(state_mask) {
            return Err(CodecError("branch node masks"));
        }
        Ok(BranchNodeCompact::new(state_mask, tree_mask, hash_mask, hashes, root_hash))
    }

    /// The packed nibble key (storage v2 `PackedStoredNibbles` / `PackedStoredNibblesSubKey`):
    /// nibbles two per byte, high nibble first, zero padded to 32 bytes, then the nibble count.
    pub fn packed_key(path: &Nibbles) -> [u8; 33] {
        assert!(path.len() <= 64);
        let mut out = [0u8; 33];
        for i in 0..path.len() {
            let nibble = path.get_unchecked(i);
            out[i / 2] |= if i % 2 == 0 { nibble << 4 } else { nibble };
        }
        out[32] = path.len() as u8;
        out
    }

    /// Decodes a packed nibble key.
    pub fn unpack_key(bytes: &[u8]) -> Decoded<Nibbles> {
        if bytes.len() < 33 || bytes[32] > 64 {
            return Err(CodecError("packed nibbles"));
        }
        let len = bytes[32] as usize;
        let nibbles: Vec<u8> = (0..len).map(|i| if i % 2 == 0 { bytes[i / 2] >> 4 } else { bytes[i / 2] & 0x0f }).collect();
        Ok(Nibbles::from_nibbles_unchecked(nibbles))
    }

    /// A `StoragesTrie` value (`PackedStorageTrieEntry`): the 33-byte packed sub-key, then the
    /// node.
    pub fn storage_entry(path: &Nibbles, node: &BranchNodeCompact) -> Vec<u8> {
        let mut out = Vec::with_capacity(33 + 6 + 32 * 17);
        out.extend_from_slice(&Self::packed_key(path));
        Self::encode_node(node, &mut out);
        out
    }

    /// Decodes a `StoragesTrie` value.
    pub fn decode_storage_entry(bytes: &[u8]) -> Decoded<(Nibbles, BranchNodeCompact)> {
        if bytes.len() < 33 {
            return Err(CodecError("storage trie entry"));
        }
        Ok((Self::unpack_key(&bytes[..33])?, Self::decode_node(&bytes[33..])?))
    }
}

/// Contract code as reth stores it in `Bytecodes`: revm's analyzed form.
#[derive(Debug)]
pub struct BytecodeCodec;

impl BytecodeCodec {
    /// The legacy analyzed variant id.
    pub const LEGACY_ANALYZED: u8 = 2;
    /// The EIP-7702 delegation variant id.
    pub const EIP7702: u8 = 4;
    /// The legacy raw variant id (only read).
    pub const LEGACY_RAW: u8 = 0;

    /// Encodes code: `u32` length of the stored (padded) bytes, the bytes, the variant, and for
    /// legacy code the original length (`u64`) and the jump-destination bitmap over the
    /// original bytes (least significant bit first, rounded up to whole bytes).
    pub fn encode(code: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(code.len() + code.len() / 8 + 48);
        if code.starts_with(&[0xef, 0x01]) {
            assert_eq!(code.len(), 23, "EIP-7702 delegation code is 23 bytes");
            out.extend_from_slice(&(code.len() as u32).to_be_bytes());
            out.extend_from_slice(code);
            out.push(Self::EIP7702);
            return out;
        }
        let (jumps, padding) = Self::analyze(code);
        out.extend_from_slice(&((code.len() + padding) as u32).to_be_bytes());
        out.extend_from_slice(code);
        out.resize(out.len() + padding, 0);
        out.push(Self::LEGACY_ANALYZED);
        out.extend_from_slice(&(code.len() as u64).to_be_bytes());
        out.extend_from_slice(&jumps);
        out
    }

    /// revm 43's legacy analysis (`analyze_legacy`): the jump-destination bitmap and how many
    /// zero bytes pad the code so execution always ends on `STOP`.
    pub fn analyze(code: &[u8]) -> (Vec<u8>, usize) {
        const JUMPDEST: u8 = 0x5b;
        const PUSH1: u8 = 0x60;
        const STOP: u8 = 0x00;
        const DUPN: u8 = 0xe6;
        let is_dupn_swapn_exchange = |op: u8| op.wrapping_sub(DUPN) < 3;
        let mut jumps = vec![0u8; code.len().div_ceil(8)];
        let (mut i, mut prev, mut last) = (0usize, 0u8, 0u8);
        while i < code.len() {
            prev = last;
            last = code[i];
            if last == JUMPDEST {
                jumps[i / 8] |= 1 << (i % 8);
                i += 1;
            } else {
                let push = last.wrapping_sub(PUSH1);
                i += if push < 32 { push as usize + 2 } else { 1 };
            }
        }
        let mut padding = i - code.len();
        if last == STOP {
            padding += usize::from(is_dupn_swapn_exchange(prev));
        } else {
            padding += 1 + usize::from(is_dupn_swapn_exchange(last));
        }
        (jumps, padding)
    }

    /// Decodes stored code back to the original bytes.
    pub fn decode(bytes: &[u8]) -> Decoded<Vec<u8>> {
        let mut reader = Reader::new(bytes);
        let len = reader.u32()? as usize;
        let stored = reader.take(len)?;
        match reader.u8()? {
            Self::LEGACY_ANALYZED => {
                let original = reader.u64_be()? as usize;
                if original > stored.len() {
                    return Err(CodecError("bytecode original length"));
                }
                Ok(stored[..original].to_vec())
            }
            Self::EIP7702 | Self::LEGACY_RAW => Ok(stored.to_vec()),
            _ => Err(CodecError("bytecode variant")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{b256, hex};

    #[test]
    fn account_matches_reth_layout() {
        // nonce 1 (1 byte), balance 0x10 (1 byte), no code: flags 0b0_000001_0001 = 0x0011.
        let account = Account { nonce: 1, balance: U256::from(0x10), bytecode_hash: None };
        assert_eq!(account.to_bytes(), hex!("1100" "01" "10"));
        // nonce 0x0102 (2 bytes), balance 0, with code: flags 2 | 0 << 4 | 1 << 10 = 0x0402.
        let hash = B256::repeat_byte(0xab);
        let account = Account { nonce: 0x0102, balance: U256::ZERO, bytecode_hash: Some(hash) };
        let mut expected = hex!("0204" "0102").to_vec();
        expected.extend_from_slice(hash.as_slice());
        assert_eq!(account.to_bytes(), expected);
        assert_eq!(Account::from_bytes(&expected).unwrap(), account);
        assert_eq!(Account::default().to_bytes(), hex!("0000"));
    }

    #[test]
    fn branch_node_round_trips_with_and_without_root() {
        let hashes = vec![B256::repeat_byte(1), B256::repeat_byte(2)];
        for root in [None, Some(B256::repeat_byte(9))] {
            let node = BranchNodeCompact::new(0xf607, 0x0005, 0x4004, hashes.clone(), root);
            let mut out = Vec::new();
            TrieCodec::encode_node(&node, &mut out);
            assert_eq!(&out[..6], hex!("f607" "0005" "4004"));
            assert_eq!(TrieCodec::decode_node(&out).unwrap(), node);
        }
    }

    #[test]
    fn packed_keys_sort_parents_first() {
        let path = |n: &[u8]| Nibbles::from_nibbles_unchecked(n.to_vec());
        assert_eq!(TrieCodec::packed_key(&path(&[0xa, 0xb, 0xc]))[..3], [0xab, 0xc0, 0]);
        assert_eq!(TrieCodec::packed_key(&path(&[0xa, 0xb, 0xc]))[32], 3);
        let keys = [path(&[]), path(&[0]), path(&[0, 0]), path(&[0, 1]), path(&[1]), path(&[1, 0, 5])];
        for pair in keys.windows(2) {
            assert!(TrieCodec::packed_key(&pair[0]) < TrieCodec::packed_key(&pair[1]));
        }
        for key in keys {
            assert_eq!(TrieCodec::unpack_key(&TrieCodec::packed_key(&key)).unwrap(), key);
        }
    }

    #[test]
    fn bytecode_analysis_matches_revm() {
        // PUSH1 1 PUSH1 2 ADD STOP: ends on STOP, no padding; no JUMPDEST.
        assert_eq!(BytecodeCodec::analyze(&hex!("6001600201 00")), (vec![0], 0));
        // Without STOP: one byte of padding.
        assert_eq!(BytecodeCodec::analyze(&hex!("6001600201")).1, 1);
        // JUMPDEST at 0 and 3; a 0x5b inside PUSH2 data is not a destination; PUSH2 overruns
        // the end by one byte: padding 1 (overrun) + 1 (no STOP).
        let code = hex!("5b 6000 5b 615b");
        assert_eq!(BytecodeCodec::analyze(&code), (vec![0b1001], 2));
        let stored = BytecodeCodec::encode(&code);
        assert_eq!(stored, hex!("00000008" "5b60005b615b0000" "02" "0000000000000006" "09"));
        assert_eq!(BytecodeCodec::decode(&stored).unwrap(), code);
        let delegation = hex!("ef0100deadbeef00000000000000000000000000000000");
        assert_eq!(BytecodeCodec::decode(&BytecodeCodec::encode(&delegation)).unwrap(), delegation);
        let _ = b256!("0000000000000000000000000000000000000000000000000000000000000000");
    }
}
