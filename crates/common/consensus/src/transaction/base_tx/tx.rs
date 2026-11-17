//! Unsigned Base transaction body ([`TxBase`]).
//!
//! This module defines the unsigned payload of a Base transaction. The
//! signed envelope (which wraps this type alongside the `sender_auth` and
//! `payer_auth` byte strings) lives in [`super::signed`].

use alloc::vec::Vec;
use core::mem;

use alloy_consensus::{InMemorySize, SignableTransaction, Transaction, Typed2718};
use alloy_eips::{eip2718::IsTyped2718, eip2930::AccessList, eip7702::SignedAuthorization};
use alloy_primitives::{
    Address, B256, Bytes, ChainId, Signature, TxKind, U256, bytes::BufMut, keccak256,
};
use alloy_rlp::{Decodable, Encodable, Header, length_of_length};
#[cfg(feature = "reth")]
use reth_codecs::Compact;

use crate::transaction::base_tx::{
    account_changes::AccountChange, call::Call, constants::TxBaseConstants,
};

/// Unsigned body of a Base transaction.
///
/// On the wire, the signed form (an [`super::TxBaseSigned`]) is
/// `BASE_TX_TYPE || rlp([...all fields..., sender_auth, payer_auth])`. The
/// unsigned struct here carries only the consensus fields; signature material
/// is held by [`super::TxBaseSigned`].
///
/// Field semantics follow the Base transaction draft. Two fields are nullable on the
/// wire (encoded as a zero-length byte string when absent):
///
/// - [`Self::sender`]: `None` selects the EOA path (recovered from
///   `sender_auth` as a 65-byte ECDSA signature); `Some` selects the
///   named-sender path with an explicit account address.
/// - [`Self::payer`]: `None` selects self-pay (the resolved sender pays);
///   [`TxBaseConstants::OPEN_PAYER`] selects open payer mode (the payer is
///   recovered from `payer_auth`); any other address selects that payer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TxBase {
    /// EIP-155 chain ID this transaction is bound to.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub chain_id: ChainId,
    /// Explicit sender account address, or `None` for the EOA path.
    pub sender: Option<Address>,
    /// Nonce channel: `0` is the account nonce, `1` through `NONCE_KEY_MAX - 1`
    /// are Nonce Manager sequences, and `NONCE_KEY_MAX` selects nonce-free mode.
    /// With `nonce_sequence` it forms the per-account replay protection key.
    pub nonce_key: U256,
    /// Sequence number within the nonce key. Named `nonce` in JSON, like every
    /// other transaction type; `nonceSequence` is also accepted.
    #[cfg_attr(
        feature = "serde",
        serde(rename = "nonce", alias = "nonceSequence", with = "alloy_serde::quantity")
    )]
    pub nonce_sequence: u64,
    /// Lower bound of the validity window: a Unix timestamp in **seconds or
    /// milliseconds** (auto-detected per EIP-8430 Timestamp Normalization; see
    /// [`Self::valid_after_ms`]). After normalization the transaction is invalid
    /// when `block.timestamp * 1000 < valid_after_ms`; `0` means no lower bound.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub valid_after: u64,
    /// Upper bound of the validity window: a Unix timestamp in **seconds or
    /// milliseconds** (auto-detected; see [`Self::valid_before_ms`]). After
    /// normalization the bound is compared against the block's exact millisecond
    /// time and is inclusive — the transaction is still valid at
    /// `block_ms == valid_before_ms` and invalid only once
    /// `block_ms > valid_before_ms` (see [`Self::is_expired_at`]); `0` means no
    /// expiry. It MUST be non-zero for nonce-free (`nonce_key == NONCE_KEY_MAX`)
    /// transactions, which additionally require `valid_before` to be strictly in
    /// the future when the nonce is recorded: the nonce-manager replay ring's
    /// admission window is `(now, now + NONCE_FREE_MAX_EXPIRY_WINDOW]`, so a
    /// nonce-free transaction cannot actually be included exactly at
    /// `valid_before`.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub valid_before: u64,
    /// Max priority fee per gas (tip) the sender is willing to pay.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_priority_fee_per_gas: u128,
    /// Max total fee per gas (base + tip cap) the sender is willing to pay.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_fee_per_gas: u128,
    /// Gas limit for the entire AA transaction execution. Named `gas` in JSON,
    /// like every other transaction type; `gasLimit` is also accepted.
    #[cfg_attr(
        feature = "serde",
        serde(rename = "gas", alias = "gasLimit", with = "alloy_serde::quantity")
    )]
    pub gas_limit: u64,
    /// Account-mutation entries applied before calls execute.
    pub account_changes: Vec<AccountChange>,
    /// Calls dispatched by the protocol after account changes apply, grouped
    /// into phases (`Vec<Vec<Call>>`).
    pub calls: Vec<Vec<Call>>,
    /// Opaque attribution/annotation bytes; empty when unused. Carried in the
    /// wire body between `calls` and `payer` and committed to by both the
    /// sender and payer signatures, but otherwise uninterpreted by the protocol.
    pub metadata: Bytes,
    /// Optional payer; `None` means the resolved sender pays gas,
    /// [`TxBaseConstants::OPEN_PAYER`] means the payer is recovered from
    /// `payer_auth`, and any other address is the named payer that `payer_auth`
    /// must authenticate.
    #[cfg_attr(
        feature = "serde",
        serde(
            default,
            serialize_with = "super::TxBasePayerSerde::serialize",
            deserialize_with = "super::TxBasePayerSerde::deserialize"
        )
    )]
    pub payer: Option<Address>,
}

impl TxBase {
    /// The lower validity bound normalized to Unix **milliseconds**.
    ///
    /// [`Self::valid_after`] may be supplied in seconds or milliseconds; this
    /// applies [`TxBaseConstants::normalize_timestamp_ms`] so callers compare
    /// against `block.timestamp * 1000` (or a millisecond wall clock) uniformly.
    /// `0` (no lower bound) is preserved. The raw field is still what the wire
    /// encoding, signature hashes, and replay id commit to; only validity-window
    /// comparisons use this normalized view.
    #[must_use]
    pub const fn valid_after_ms(&self) -> u64 {
        TxBaseConstants::normalize_timestamp_ms(self.valid_after)
    }

    /// The upper validity bound normalized to Unix **milliseconds**.
    ///
    /// [`Self::valid_before`] may be supplied in seconds or milliseconds; this
    /// applies [`TxBaseConstants::normalize_timestamp_ms`]. `0` (no expiry) is
    /// preserved. As with [`Self::valid_after_ms`], the raw field remains the
    /// signed/encoded/replay-committed value; this normalized view is used only
    /// for validity-window and nonce-free expiry comparisons.
    #[must_use]
    pub const fn valid_before_ms(&self) -> u64 {
        TxBaseConstants::normalize_timestamp_ms(self.valid_before)
    }

    /// The last block time, in Unix **milliseconds**, at which `valid_before`
    /// still admits the transaction, or `None` when it has no expiry.
    ///
    /// The block time is the block's exact millisecond timestamp
    /// (`block.timestamp * 1000` plus the `BaseTime` millisecond part). A
    /// nonce-bearing transaction is valid through `valid_before_ms` inclusive.
    /// A nonce-free transaction must be recorded by the nonce-manager replay
    /// ring, which only accepts a strictly-future `valid_before`, so its last
    /// valid millisecond is `valid_before_ms - 1`.
    #[must_use]
    pub fn last_valid_ms(&self) -> Option<u64> {
        let valid_before = self.valid_before_ms();
        if valid_before == 0 {
            None
        } else if self.nonce_key == TxBaseConstants::NONCE_KEY_MAX {
            Some(valid_before - 1)
        } else {
            Some(valid_before)
        }
    }

    /// Whether `valid_before` has passed at the block millisecond time `now_ms`.
    ///
    /// This is the single Base transaction expiry rule: admission, mempool eviction,
    /// the builder precheck and execution all decide expiry with it. See
    /// [`Self::last_valid_ms`].
    #[must_use]
    pub fn is_expired_at(&self, now_ms: u64) -> bool {
        self.last_valid_ms().is_some_and(|last_valid| now_ms > last_valid)
    }

    /// Encodes an `Option<Address>` as the AA wire format: zero-length byte
    /// string when `None`, 20-byte string when `Some`.
    fn encode_address_opt(addr: &Option<Address>, out: &mut dyn BufMut) {
        match addr {
            None => Bytes::new().encode(out),
            Some(a) => Bytes::copy_from_slice(a.as_slice()).encode(out),
        }
    }

    /// Length contribution of an `Option<Address>` under [`Self::encode_address_opt`].
    const fn address_opt_encoded_length(addr: &Option<Address>) -> usize {
        match addr {
            None => 1,
            Some(_) => 21,
        }
    }

    /// Decodes the [`Self::encode_address_opt`] wire format.
    fn decode_address_opt(buf: &mut &[u8]) -> alloy_rlp::Result<Option<Address>> {
        let raw = Bytes::decode(buf)?;
        match raw.len() {
            0 => Ok(None),
            20 => Ok(Some(Address::from_slice(&raw))),
            _ => Err(alloy_rlp::Error::Custom("invalid Option<Address> length")),
        }
    }

    /// Encodes `payer`: empty for self-pay, the single byte `0x00` for open
    /// payer mode ([`TxBaseConstants::OPEN_PAYER`]), otherwise the 20-byte
    /// address.
    fn encode_payer(payer: Option<Address>, out: &mut dyn BufMut) {
        match payer {
            Some(TxBaseConstants::OPEN_PAYER) => [0u8].as_slice().encode(out),
            payer => Self::encode_address_opt(&payer, out),
        }
    }

    /// Length contribution of `payer` under [`Self::encode_payer`].
    const fn payer_encoded_length(payer: Option<Address>) -> usize {
        match payer {
            Some(TxBaseConstants::OPEN_PAYER) => 1,
            payer => Self::address_opt_encoded_length(&payer),
        }
    }

    /// Decodes the [`Self::encode_payer`] wire format. A 20-byte zero address
    /// is rejected so open payer mode has a single encoding.
    fn decode_payer(buf: &mut &[u8]) -> alloy_rlp::Result<Option<Address>> {
        let raw = Bytes::decode(buf)?;
        match raw.as_ref() {
            [] => Ok(None),
            [0] => Ok(Some(TxBaseConstants::OPEN_PAYER)),
            bytes if bytes.len() == 20 && bytes != [0u8; 20] => {
                Ok(Some(Address::from_slice(bytes)))
            }
            _ => Err(alloy_rlp::Error::Custom("invalid Base transaction payer encoding")),
        }
    }

    /// Whether the transaction uses open payer mode: the sender does not name a
    /// payer, and any key willing to pay signs `payer_auth`.
    #[must_use]
    pub fn is_open_payer(&self) -> bool {
        self.payer == Some(TxBaseConstants::OPEN_PAYER)
    }

    /// Length of the RLP fields that precede the metered payload in wire order,
    /// so the payload starts at this offset within [`Self::rlp_encode_fields`].
    pub fn metered_payload_offset(&self) -> usize {
        self.chain_id.length()
            + Self::address_opt_encoded_length(&self.sender)
            + self.nonce_key.length()
            + self.nonce_sequence.length()
            + self.valid_after.length()
            + self.valid_before.length()
            + self.max_priority_fee_per_gas.length()
            + self.max_fee_per_gas.length()
            + self.gas_limit.length()
    }

    /// Length of the bytes [`Self::encode_metered_payload`] writes.
    pub fn metered_payload_length(&self) -> usize {
        self.account_changes.length()
            + Self::calls_encoded_length(&self.calls)
            + self.metadata.length()
    }

    /// Encodes the fields `tx_payload_cost` meters: the RLP of
    /// `account_changes`, `calls` and `metadata`, in wire order. The envelope
    /// is excluded so the cost does not depend on `gas_limit`, the fees or the
    /// signatures that commit to them.
    pub fn encode_metered_payload(&self, out: &mut dyn BufMut) {
        self.account_changes.encode(out);
        Self::encode_calls(&self.calls, out);
        self.metadata.encode(out);
    }

    /// Encodes the inner phase list of `calls` as `rlp([rlp([Call, ...]), ...])`.
    fn encode_calls(calls: &[Vec<Call>], out: &mut dyn BufMut) {
        let mut payload_len = 0usize;
        for phase in calls {
            payload_len += phase.length();
        }
        Header { list: true, payload_length: payload_len }.encode(out);
        for phase in calls {
            phase.encode(out);
        }
    }

    /// Total RLP length of the `calls` field as encoded by [`Self::encode_calls`].
    fn calls_encoded_length(calls: &[Vec<Call>]) -> usize {
        let mut payload_len = 0usize;
        for phase in calls {
            payload_len += phase.length();
        }
        length_of_length(payload_len) + payload_len
    }

    /// Decodes the `calls` phase list, rejecting more than
    /// [`TxBaseConstants::MAX_CALL_PHASES_PER_TX`] phases before allocating
    /// the excess. Every decoder of a Base transaction `calls` field must use this so
    /// all wire formats accept the same transactions.
    pub fn decode_calls(buf: &mut &[u8]) -> alloy_rlp::Result<Vec<Vec<Call>>> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        if buf.len() < header.payload_length {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        let (mut payload, rest) = buf.split_at(header.payload_length);
        let mut phases = Vec::new();
        while !payload.is_empty() {
            if phases.len() >= TxBaseConstants::MAX_CALL_PHASES_PER_TX {
                return Err(alloy_rlp::Error::Custom("too many Base transaction call phases"));
            }
            phases.push(Vec::<Call>::decode(&mut payload)?);
        }
        *buf = rest;
        Ok(phases)
    }

    /// Length of all RLP fields (no list header), encoding `sender` in place of
    /// the transaction's stored sender.
    fn rlp_encoded_fields_length_with_sender(&self, sender: &Option<Address>) -> usize {
        self.chain_id.length()
            + Self::address_opt_encoded_length(sender)
            + self.nonce_key.length()
            + self.nonce_sequence.length()
            + self.valid_after.length()
            + self.valid_before.length()
            + self.max_priority_fee_per_gas.length()
            + self.max_fee_per_gas.length()
            + self.gas_limit.length()
            + self.account_changes.length()
            + Self::calls_encoded_length(&self.calls)
            + self.metadata.length()
            + Self::payer_encoded_length(self.payer)
    }

    /// Encodes the RLP fields (no list header) in canonical order, encoding
    /// `sender` in place of the transaction's stored sender.
    fn rlp_encode_fields_with_sender(&self, sender: &Option<Address>, out: &mut dyn BufMut) {
        self.chain_id.encode(out);
        Self::encode_address_opt(sender, out);
        self.nonce_key.encode(out);
        self.nonce_sequence.encode(out);
        self.valid_after.encode(out);
        self.valid_before.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
        self.gas_limit.encode(out);
        self.account_changes.encode(out);
        Self::encode_calls(&self.calls, out);
        self.metadata.encode(out);
        Self::encode_payer(self.payer, out);
    }

    /// Length of all RLP fields (no list header).
    pub fn rlp_encoded_fields_length(&self) -> usize {
        self.rlp_encoded_fields_length_with_sender(&self.sender)
    }

    /// Encodes the RLP fields (no list header) in canonical order.
    pub fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.rlp_encode_fields_with_sender(&self.sender, out);
    }

    /// Decodes the RLP fields (no list header) in canonical order.
    pub fn rlp_decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self {
            chain_id: Decodable::decode(buf)?,
            sender: Self::decode_address_opt(buf)?,
            nonce_key: Decodable::decode(buf)?,
            nonce_sequence: Decodable::decode(buf)?,
            valid_after: Decodable::decode(buf)?,
            valid_before: Decodable::decode(buf)?,
            max_priority_fee_per_gas: Decodable::decode(buf)?,
            max_fee_per_gas: Decodable::decode(buf)?,
            gas_limit: Decodable::decode(buf)?,
            account_changes: Decodable::decode(buf)?,
            calls: Self::decode_calls(buf)?,
            metadata: Decodable::decode(buf)?,
            payer: Self::decode_payer(buf)?,
        })
    }

    fn rlp_header(&self) -> Header {
        Header { list: true, payload_length: self.rlp_encoded_fields_length() }
    }

    /// RLP-encodes the unsigned transaction body (with list header).
    pub fn rlp_encode(&self, out: &mut dyn BufMut) {
        self.rlp_header().encode(out);
        self.rlp_encode_fields(out);
    }

    /// Returns the RLP-encoded length of the unsigned transaction body.
    pub fn rlp_encoded_length(&self) -> usize {
        self.rlp_header().length_with_payload()
    }

    #[cfg(feature = "reth")]
    fn compact_tail(&self) -> Vec<u8> {
        let mut calls = Vec::with_capacity(Self::calls_encoded_length(&self.calls));
        Self::encode_calls(&self.calls, &mut calls);

        let account_changes = Bytes::from(alloy_rlp::encode(&self.account_changes));
        let calls = Bytes::from(calls);
        let tail_payload_length =
            account_changes.length() + calls.length() + self.metadata.length();
        let mut tail =
            Vec::with_capacity(length_of_length(tail_payload_length) + tail_payload_length);
        Header { list: true, payload_length: tail_payload_length }.encode(&mut tail);
        account_changes.encode(&mut tail);
        calls.encode(&mut tail);
        self.metadata.encode(&mut tail);
        tail
    }

    #[cfg(feature = "reth")]
    fn decode_compact_tail(tail: &[u8]) -> (Vec<AccountChange>, Vec<Vec<Call>>, Bytes) {
        let mut tail = tail;
        let header = Header::decode(&mut tail)
            .unwrap_or_else(|err| panic!("invalid compact-encoded Base transaction tail: {err}"));
        assert!(header.list, "compact-encoded Base transaction tail must be an RLP list");
        let started = tail.len();

        let account_changes = Bytes::decode(&mut tail).unwrap_or_else(|err| {
            panic!("invalid compact-encoded Base transaction account changes: {err}")
        });
        let calls = Bytes::decode(&mut tail)
            .unwrap_or_else(|err| panic!("invalid compact-encoded Base transaction calls: {err}"));
        let metadata = Bytes::decode(&mut tail).unwrap_or_else(|err| {
            panic!("invalid compact-encoded Base transaction metadata: {err}")
        });
        let consumed = started - tail.len();
        assert_eq!(
            consumed, header.payload_length,
            "compact-encoded Base transaction tail length mismatch"
        );

        let mut account_changes_buf = account_changes.as_ref();
        let account_changes = Vec::<AccountChange>::decode(&mut account_changes_buf)
            .unwrap_or_else(|err| {
                panic!("invalid compact-encoded Base transaction account changes: {err}")
            });
        assert!(
            account_changes_buf.is_empty(),
            "compact-encoded Base transaction account changes left trailing bytes"
        );

        let mut calls_buf = calls.as_ref();
        let calls = Self::decode_calls(&mut calls_buf)
            .unwrap_or_else(|err| panic!("invalid compact-encoded Base transaction calls: {err}"));
        assert!(calls_buf.is_empty(), "compact-encoded Base transaction calls left trailing bytes");

        (account_changes, calls, metadata)
    }

    /// Hash the sender signs, per EIP-8430.
    ///
    /// `keccak256(SENDER_SIGNATURE_TYPE || rlp([all body fields through `payer`]))`
    /// with the wire `sender` slot, which is empty on the EOA path, where the
    /// sender is recovered from `sender_auth`. `sender_auth` and `payer_auth`
    /// are excluded.
    pub fn sender_signature_hash(&self) -> B256 {
        self.domain_hash(&TxBaseConstants::SENDER_SIGNATURE_TYPE, &self.sender)
    }

    /// Returns the fee- and signature-invariant replay identifier.
    ///
    /// Used only for nonce-free (`nonce_key == NONCE_KEY_MAX`) transactions, which
    /// always carry `(NONCE_KEY_MAX, 0)`; `nonce_key`/`nonce_sequence` therefore
    /// carry no entropy and are omitted from the preimage. The resolved sender is
    /// encoded in place of the nullable wire sender:
    /// `keccak256(REPLAY_ID_TYPE || rlp([chain_id, resolved_sender, valid_after,
    /// valid_before, account_changes, calls, metadata, payer]))`.
    #[must_use]
    pub fn replay_id(&self, resolved_sender: Address) -> B256 {
        let payload_length = self.chain_id.length()
            + resolved_sender.length()
            + self.valid_after.length()
            + self.valid_before.length()
            + self.account_changes.length()
            + Self::calls_encoded_length(&self.calls)
            + self.metadata.length()
            + Self::payer_encoded_length(self.payer);
        let mut buf = Vec::with_capacity(
            TxBaseConstants::REPLAY_ID_TYPE.len()
                + length_of_length(payload_length)
                + payload_length,
        );
        buf.put_slice(&TxBaseConstants::REPLAY_ID_TYPE);
        Header { list: true, payload_length }.encode(&mut buf);
        self.chain_id.encode(&mut buf);
        resolved_sender.encode(&mut buf);
        self.valid_after.encode(&mut buf);
        self.valid_before.encode(&mut buf);
        self.account_changes.encode(&mut buf);
        Self::encode_calls(&self.calls, &mut buf);
        self.metadata.encode(&mut buf);
        Self::encode_payer(self.payer, &mut buf);
        keccak256(&buf)
    }

    /// Hash the payer signs, per EIP-8430.
    ///
    /// `keccak256(PAYER_SIGNATURE_TYPE || rlp([all body fields through `payer`]))`
    /// with the `sender` slot replaced by the resolved sender address. The
    /// payer commits to the full transaction body — including the `payer` slot
    /// itself — and only the `sender_auth` / `payer_auth` slots (which live in
    /// the signed wrapper) are excluded.
    pub fn payer_signature_hash(&self, resolved_sender: Address) -> B256 {
        self.domain_hash(&TxBaseConstants::PAYER_SIGNATURE_TYPE, &Some(resolved_sender))
    }

    /// `keccak256(domain || rlp([all body fields through `payer`]))` with
    /// `sender` in the `sender` slot.
    fn domain_hash(&self, domain: &[u8], sender: &Option<Address>) -> B256 {
        let payload_length = self.rlp_encoded_fields_length_with_sender(sender);
        let mut buf =
            Vec::with_capacity(domain.len() + length_of_length(payload_length) + payload_length);
        buf.put_slice(domain);
        Header { list: true, payload_length }.encode(&mut buf);
        self.rlp_encode_fields_with_sender(sender, &mut buf);
        keccak256(&buf)
    }

    /// In-memory size heuristic.
    pub fn size(&self) -> usize {
        mem::size_of::<Self>()
            + self.account_changes.capacity().saturating_mul(mem::size_of::<AccountChange>())
            + self.calls.capacity().saturating_mul(mem::size_of::<Vec<Call>>())
            + self
                .calls
                .iter()
                .map(|phase| {
                    phase.capacity().saturating_mul(mem::size_of::<Call>())
                        + phase.iter().map(Call::heap_size).sum::<usize>()
                })
                .sum::<usize>()
            + self.metadata.len()
    }
}

impl Encodable for TxBase {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_encode(out);
    }

    fn length(&self) -> usize {
        self.rlp_encoded_length()
    }
}

impl Decodable for TxBase {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let started = buf.len();
        let this = Self::rlp_decode_fields(buf)?;
        let consumed = started - buf.len();
        if consumed != header.payload_length {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: consumed,
            });
        }
        Ok(this)
    }
}

#[cfg(feature = "reth")]
#[derive(Debug, Clone, PartialEq, Eq, Compact)]
#[reth_codecs(crate = "reth_codecs")]
struct CompactTxBaseHead {
    chain_id: ChainId,
    sender: Option<Address>,
    nonce_key: U256,
    nonce_sequence: u64,
    valid_after: u64,
    valid_before: u64,
    max_priority_fee_per_gas: u128,
    max_fee_per_gas: u128,
    gas_limit: u64,
    payer: Option<Address>,
    tail_len: u64,
}

#[cfg(feature = "reth")]
impl Compact for TxBase {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let tail = self.compact_tail();
        let head = CompactTxBaseHead {
            chain_id: self.chain_id,
            sender: self.sender,
            nonce_key: self.nonce_key,
            nonce_sequence: self.nonce_sequence,
            valid_after: self.valid_after,
            valid_before: self.valid_before,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas,
            max_fee_per_gas: self.max_fee_per_gas,
            gas_limit: self.gas_limit,
            payer: self.payer,
            tail_len: tail.len() as u64,
        };

        let identifier = head.to_compact(buf);
        buf.put_slice(&tail);
        identifier
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (head, buf) = CompactTxBaseHead::from_compact(buf, len);
        let CompactTxBaseHead {
            chain_id,
            sender,
            nonce_key,
            nonce_sequence,
            valid_after,
            valid_before,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            gas_limit,
            payer,
            tail_len,
        } = head;

        let tail_len: usize =
            tail_len.try_into().expect("Base transaction compact tail length overflow");
        assert!(
            buf.len() >= tail_len,
            "compact-encoded Base transaction tail shorter than declared length"
        );
        let (tail, buf) = buf.split_at(tail_len);
        let (account_changes, calls, metadata) = Self::decode_compact_tail(tail);

        (
            Self {
                chain_id,
                sender,
                nonce_key,
                nonce_sequence,
                valid_after,
                valid_before,
                max_priority_fee_per_gas,
                max_fee_per_gas,
                gas_limit,
                account_changes,
                calls,
                metadata,
                payer,
            },
            buf,
        )
    }
}

impl Typed2718 for TxBase {
    fn ty(&self) -> u8 {
        TxBaseConstants::BASE_TX_TYPE
    }
}

impl IsTyped2718 for TxBase {
    fn is_type(ty: u8) -> bool {
        ty == TxBaseConstants::BASE_TX_TYPE
    }
}

impl InMemorySize for TxBase {
    fn size(&self) -> usize {
        Self::size(self)
    }
}

impl Transaction for TxBase {
    fn chain_id(&self) -> Option<ChainId> {
        Some(self.chain_id)
    }

    fn nonce(&self) -> u64 {
        self.nonce_sequence
    }

    fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    fn gas_price(&self) -> Option<u128> {
        None
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        Some(self.max_priority_fee_per_gas)
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        None
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.max_priority_fee_per_gas
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        base_fee.map_or(self.max_fee_per_gas, |bf| {
            (bf as u128).saturating_add(self.max_priority_fee_per_gas).min(self.max_fee_per_gas)
        })
    }

    fn is_dynamic_fee(&self) -> bool {
        true
    }

    fn kind(&self) -> TxKind {
        TxKind::Call(Address::ZERO)
    }

    fn is_create(&self) -> bool {
        false
    }

    fn value(&self) -> U256 {
        U256::ZERO
    }

    fn input(&self) -> &Bytes {
        static EMPTY: Bytes = Bytes::new();
        &EMPTY
    }

    fn access_list(&self) -> Option<&AccessList> {
        None
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        None
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        None
    }
}

impl SignableTransaction<Signature> for TxBase {
    fn set_chain_id(&mut self, chain_id: ChainId) {
        self.chain_id = chain_id;
    }

    /// The sender signature preimage, whose keccak256 is
    /// [`TxBase::sender_signature_hash`].
    fn encode_for_signing(&self, out: &mut dyn BufMut) {
        out.put_slice(&TxBaseConstants::SENDER_SIGNATURE_TYPE);
        self.rlp_encode(out);
    }

    fn payload_len_for_signature(&self) -> usize {
        TxBaseConstants::SENDER_SIGNATURE_TYPE.len() + self.rlp_encoded_length()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, bytes};
    #[cfg(feature = "reth")]
    use reth_codecs::Compact;
    #[cfg(feature = "reth")]
    use rstest::rstest;

    use super::*;
    use crate::transaction::base_tx::account_changes::Delegation;

    fn sample_tx() -> TxBase {
        TxBase {
            chain_id: 8453,
            sender: Some(address!("0x00000000000000000000000000000000000000aa")),
            nonce_key: U256::from(0x1234u64),
            nonce_sequence: 7,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 200_000,
            account_changes: vec![AccountChange::Delegation(Delegation {
                target: address!("0x00000000000000000000000000000000000000bb"),
            })],
            calls: vec![vec![Call {
                to: address!("0x00000000000000000000000000000000000000cc"),
                value: U256::from(0x1234u64),
                data: bytes!("deadbeef"),
            }]],
            metadata: bytes!("c0ffee"),
            payer: None,
        }
    }

    fn empty_call_phases(count: usize) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(length_of_length(count) + count);
        Header { list: true, payload_length: count }.encode(&mut encoded);
        encoded.resize(encoded.len() + count, 0xc0);
        encoded
    }

    #[test]
    fn validity_bounds_normalize_seconds_to_milliseconds() {
        let mut tx = sample_tx();
        // Zero (disabled) is preserved on both bounds.
        tx.valid_after = 0;
        tx.valid_before = 0;
        assert_eq!(tx.valid_after_ms(), 0);
        assert_eq!(tx.valid_before_ms(), 0);
        // Seconds (< threshold) are scaled by 1000.
        tx.valid_after = 1_700_000_000;
        tx.valid_before = 1_700_000_020;
        assert_eq!(tx.valid_after_ms(), 1_700_000_000_000);
        assert_eq!(tx.valid_before_ms(), 1_700_000_020_000);
        // Milliseconds (>= threshold) pass through unchanged.
        tx.valid_after = 1_700_000_000_000;
        tx.valid_before = 1_700_000_020_000;
        assert_eq!(tx.valid_after_ms(), 1_700_000_000_000);
        assert_eq!(tx.valid_before_ms(), 1_700_000_020_000);
    }

    #[test]
    fn expiry_is_inclusive_for_nonce_bearing_and_exclusive_for_nonce_free() {
        let mut tx = sample_tx();
        tx.valid_before = 0;
        assert_eq!(tx.last_valid_ms(), None);
        assert!(!tx.is_expired_at(u64::MAX));

        // Sub-second bound: five 200 ms blocks share header second 1_700_000_000.
        tx.valid_before = 1_700_000_000_300;
        assert_eq!(tx.last_valid_ms(), Some(1_700_000_000_300));
        assert!(!tx.is_expired_at(1_700_000_000_200));
        assert!(!tx.is_expired_at(1_700_000_000_300));
        assert!(tx.is_expired_at(1_700_000_000_301));

        tx.nonce_key = TxBaseConstants::NONCE_KEY_MAX;
        assert_eq!(tx.last_valid_ms(), Some(1_700_000_000_299));
        assert!(!tx.is_expired_at(1_700_000_000_299));
        assert!(tx.is_expired_at(1_700_000_000_300));

        // A seconds bound is normalized before the comparison.
        tx.nonce_key = U256::ZERO;
        tx.valid_before = 1_700_000_020;
        assert!(!tx.is_expired_at(1_700_000_020_000));
        assert!(tx.is_expired_at(1_700_000_020_001));
    }

    #[test]
    fn decode_calls_accepts_maximum_phase_count() {
        let encoded = empty_call_phases(TxBaseConstants::MAX_CALL_PHASES_PER_TX);
        let mut input = encoded.as_slice();

        let phases = TxBase::decode_calls(&mut input).unwrap();

        assert_eq!(phases.len(), TxBaseConstants::MAX_CALL_PHASES_PER_TX);
        assert!(phases.iter().all(Vec::is_empty));
        assert!(input.is_empty());
    }

    #[test]
    fn decode_calls_rejects_excessive_phase_count() {
        let encoded = empty_call_phases(TxBaseConstants::MAX_CALL_PHASES_PER_TX + 1);
        let mut input = encoded.as_slice();

        let error = TxBase::decode_calls(&mut input).unwrap_err();

        assert_eq!(error, alloy_rlp::Error::Custom("too many Base transaction call phases"));
    }

    #[test]
    fn rlp_roundtrip_full() {
        let tx = sample_tx();
        let mut buf = Vec::new();
        tx.rlp_encode(&mut buf);
        assert_eq!(buf.len(), tx.rlp_encoded_length());
        let decoded = TxBase::rlp_decode_fields(&mut {
            let header = Header::decode(&mut &buf[..]).unwrap();
            assert!(header.list);
            &buf[buf.len() - header.payload_length..]
        })
        .unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn rlp_roundtrip_via_decodable() {
        let tx = sample_tx();
        let mut buf = Vec::new();
        tx.encode(&mut buf);
        let decoded = TxBase::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    #[cfg(feature = "reth")]
    #[rstest]
    #[case::self_pay(sample_tx(), &[])]
    #[case::eoa_sender_with_explicit_payer(
        TxBase {
            sender: None,
            payer: Some(address!("0x00000000000000000000000000000000000000dd")),
            ..sample_tx()
        },
        &[],
    )]
    #[case::trailing_bytes(sample_tx(), &[0xDE, 0xAD, 0xBE, 0xEF])]
    fn compact_roundtrip(#[case] tx: TxBase, #[case] trailing: &[u8]) {
        let mut buf = Vec::new();
        let _ = tx.to_compact(&mut buf);
        buf.extend_from_slice(trailing);

        let (decoded, remaining) = TxBase::from_compact(&buf, buf.len());

        assert_eq!(decoded, tx);
        assert_eq!(remaining, trailing);
    }

    #[test]
    fn rlp_roundtrip_minimal_empty() {
        let tx = TxBase::default();
        let mut buf = Vec::new();
        tx.encode(&mut buf);
        let decoded = TxBase::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn address_opt_roundtrip_none() {
        let mut buf = Vec::new();
        TxBase::encode_address_opt(&None, &mut buf);
        assert_eq!(buf, vec![0x80]);
        let decoded = TxBase::decode_address_opt(&mut buf.as_slice()).unwrap();
        assert_eq!(decoded, None);
    }

    #[test]
    fn address_opt_roundtrip_some() {
        let addr = address!("0x00000000000000000000000000000000000000ff");
        let mut buf = Vec::new();
        TxBase::encode_address_opt(&Some(addr), &mut buf);
        let decoded = TxBase::decode_address_opt(&mut buf.as_slice()).unwrap();
        assert_eq!(decoded, Some(addr));
    }

    #[test]
    fn address_opt_rejects_wrong_length() {
        let mut buf = Vec::new();
        Bytes::copy_from_slice(&[0u8; 19]).encode(&mut buf);
        let res = TxBase::decode_address_opt(&mut buf.as_slice());
        assert!(res.is_err());
    }

    #[test]
    fn open_payer_roundtrips_as_single_zero_byte() {
        let tx = TxBase { payer: Some(TxBaseConstants::OPEN_PAYER), ..sample_tx() };
        assert!(tx.is_open_payer());

        let mut payer = Vec::new();
        TxBase::encode_payer(tx.payer, &mut payer);
        assert_eq!(payer, vec![0x00]);
        assert_eq!(TxBase::payer_encoded_length(tx.payer), 1);

        let mut buf = Vec::new();
        tx.encode(&mut buf);
        assert_eq!(buf.len(), tx.length());
        assert_eq!(TxBase::decode(&mut buf.as_slice()).unwrap(), tx);
    }

    #[test]
    fn payer_rejects_zero_address_and_other_lengths() {
        for raw in [vec![0u8; 20], vec![1u8], vec![0u8; 19]] {
            let mut buf = Vec::new();
            Bytes::from(raw).encode(&mut buf);
            assert!(TxBase::decode_payer(&mut buf.as_slice()).is_err());
        }
    }

    #[test]
    fn signing_hashes_are_distinct() {
        let tx = sample_tx();
        let sender_hash = tx.sender_signature_hash();
        let payer_hash =
            tx.payer_signature_hash(address!("0x00000000000000000000000000000000000000dd"));
        assert_ne!(sender_hash, payer_hash);
    }

    #[test]
    fn replay_id_ignores_fee_fields() {
        let tx = sample_tx();
        let sender = tx.sender.unwrap();
        let mut other = tx.clone();
        other.max_priority_fee_per_gas += 1;
        other.max_fee_per_gas += 2;
        other.gas_limit += 3;
        assert_eq!(tx.replay_id(sender), other.replay_id(sender));
    }

    #[test]
    fn replay_id_ignores_nonce_fields() {
        // Nonce-free txs always carry `(NONCE_KEY_MAX, 0)`, so `nonce_key` and
        // `nonce_sequence` are omitted from the preimage and cannot alter identity.
        let tx = sample_tx();
        let sender = tx.sender.unwrap();
        let mut other = tx.clone();
        other.nonce_key = U256::from(7u64);
        other.nonce_sequence = 42;
        assert_eq!(tx.replay_id(sender), other.replay_id(sender));
    }

    #[test]
    fn replay_id_commits_to_payer_calls_and_validity_window() {
        let tx = sample_tx();
        let sender = tx.sender.unwrap();

        let mut changed = tx.clone();
        changed.payer = Some(address!("0x00000000000000000000000000000000000000dd"));
        assert_ne!(tx.replay_id(sender), changed.replay_id(sender));

        changed = tx.clone();
        changed.calls[0][0].data = bytes!("01");
        assert_ne!(tx.replay_id(sender), changed.replay_id(sender));

        changed = tx.clone();
        changed.valid_after = 1;
        assert_ne!(tx.replay_id(sender), changed.replay_id(sender));

        changed = tx.clone();
        changed.valid_before = 1;
        assert_ne!(tx.replay_id(sender), changed.replay_id(sender));
    }

    #[test]
    fn ty_byte_matches_constant() {
        assert_eq!(sample_tx().ty(), TxBaseConstants::BASE_TX_TYPE);
        assert!(<TxBase as IsTyped2718>::is_type(TxBaseConstants::BASE_TX_TYPE));
        assert!(!<TxBase as IsTyped2718>::is_type(0x00));
    }

    #[test]
    fn nested_calls_roundtrip() {
        let tx = TxBase {
            chain_id: 1,
            calls: vec![
                vec![Call { to: Address::ZERO, value: U256::from(7u64), data: bytes!("01") }],
                vec![],
                vec![
                    Call { to: Address::ZERO, value: U256::ZERO, data: bytes!("02") },
                    Call { to: Address::ZERO, value: U256::ZERO, data: bytes!("03") },
                ],
            ],
            ..Default::default()
        };
        let mut buf = Vec::new();
        tx.encode(&mut buf);
        let decoded = TxBase::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn call_value_is_covered_by_signature_hash() {
        let mut tx = TxBase {
            chain_id: 1,
            calls: vec![vec![Call { to: Address::ZERO, value: U256::ZERO, data: bytes!("01") }]],
            ..Default::default()
        };
        let zero_value_hash = tx.sender_signature_hash();
        tx.calls[0][0].value = U256::from(1u64);
        let nonzero_value_hash = tx.sender_signature_hash();
        assert_ne!(
            zero_value_hash, nonzero_value_hash,
            "call value must be part of the signed preimage",
        );
    }

    #[test]
    fn account_change_roundtrip_in_tx() {
        let tx = TxBase {
            chain_id: 1,
            account_changes: vec![
                AccountChange::Delegation(Delegation { target: Address::ZERO }),
                AccountChange::Delegation(Delegation {
                    target: address!("0x00000000000000000000000000000000000000ee"),
                }),
            ],
            ..Default::default()
        };
        let mut buf = Vec::new();
        tx.encode(&mut buf);
        let decoded = TxBase::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx.account_changes, decoded.account_changes);
    }

    #[test]
    fn payer_signature_hash_uses_substituted_sender() {
        let mut tx = sample_tx();
        tx.sender = None;
        let resolved = address!("0x00000000000000000000000000000000000000dd");
        let payer_hash_v1 = tx.payer_signature_hash(resolved);

        let tx2 = TxBase { sender: Some(resolved), ..tx };
        let mut buf = Vec::with_capacity(tx2.rlp_encoded_length() + 2);
        buf.put_slice(&TxBaseConstants::PAYER_SIGNATURE_TYPE);
        tx2.rlp_encode(&mut buf);
        let payer_hash_v2 = keccak256(&buf);
        assert_eq!(payer_hash_v1, payer_hash_v2);
    }

    // Each signing hash is keccak256 of its domain prefix and the RLP body, and
    // the domains keep them apart even when the bodies are identical.
    #[test]
    fn hashes_prefix_the_body_with_their_domain() {
        let tx = sample_tx();
        let sender = tx.sender.unwrap();
        let hash_with = |domain: &[u8]| {
            let mut buf = domain.to_vec();
            tx.rlp_encode(&mut buf);
            keccak256(&buf)
        };

        assert_eq!(tx.sender_signature_hash(), hash_with(&[0x79, 0x00]));
        assert_eq!(tx.payer_signature_hash(sender), hash_with(&[0x79, 0x01]));
        assert_ne!(tx.sender_signature_hash(), tx.payer_signature_hash(sender));
        assert_eq!(tx.signature_hash(), tx.sender_signature_hash());
    }

    // `metadata` is committed to by both signatures.
    #[test]
    fn metadata_is_covered_by_both_signature_hashes() {
        let tx = sample_tx();
        let resolved = address!("0x00000000000000000000000000000000000000dd");
        let mut other = tx.clone();
        other.metadata = bytes!("beef");

        assert_ne!(tx.sender_signature_hash(), other.sender_signature_hash());
        assert_ne!(tx.payer_signature_hash(resolved), other.payer_signature_hash(resolved));
    }

    // Both signatures commit to the full body through `payer`, so changing the
    // `payer` slot changes both the sender and payer hashes.
    #[test]
    fn payer_field_is_covered_by_both_signature_hashes() {
        let resolved = address!("0x00000000000000000000000000000000000000dd");
        let mut self_pay = sample_tx();
        self_pay.payer = None;
        let sponsored = TxBase {
            payer: Some(address!("0x00000000000000000000000000000000000000ee")),
            ..self_pay.clone()
        };

        assert_ne!(
            self_pay.payer_signature_hash(resolved),
            sponsored.payer_signature_hash(resolved)
        );
        assert_ne!(self_pay.sender_signature_hash(), sponsored.sender_signature_hash());
    }

    /// Proves that Base transactions can be deserialized through the
    /// `BaseTxEnvelope` tagged-enum path with hex-string quantity fields.
    ///
    /// Without `#[serde(with = "alloy_serde::quantity")]` on the u128 fee
    /// fields, this test fails with:
    ///   "u128 is not supported"
    /// because serde's internal Content buffer (used for internally-tagged
    /// enums) cannot handle u128 deserialization.
    ///
    /// See: <https://github.com/serde-rs/serde/issues/2230>
    #[cfg(feature = "serde")]
    #[test]
    fn base_tx_deserializes_through_envelope() {
        use crate::BaseTxEnvelope;

        let json = serde_json::json!({
            "type": "0x79",
            "chainId": "0x1",
            "sender": null,
            "nonceKey": "0x0",
            "nonce": "0x1",
            "validAfter": "0x0",
            "validBefore": "0x0",
            "maxPriorityFeePerGas": "0x3b9aca00",
            "maxFeePerGas": "0x3b9aca00",
            "gas": "0x5208",
            "accountChanges": [],
            "calls": [],
            "metadata": "0x",
            "payer": null,
            "senderAuth": "0x",
            "payerAuth": "0x"
        });

        let result = serde_json::from_value::<BaseTxEnvelope>(json);
        assert!(
            result.is_ok(),
            "Base transaction envelope deserialization failed: {:#}",
            result.unwrap_err()
        );

        let envelope = result.unwrap();
        assert!(matches!(envelope, BaseTxEnvelope::Base(_)));
    }

    /// The JSON is flat and uses hex quantities for every integer, with the
    /// standard `nonce` / `gas` names and single-call fields generic tooling
    /// reads. An open payer is `"0x00"`, its wire form. The EIP field names
    /// `nonceSequence` / `gasLimit` are also accepted.
    #[cfg(feature = "serde")]
    #[test]
    fn base_tx_json_is_a_standard_transaction_shape() {
        use crate::{BaseTxEnvelope, TxBaseSigned};

        let tx = TxBase {
            chain_id: 8453,
            nonce_sequence: 7,
            valid_before: 1_700_000_000,
            gas_limit: 21_000,
            payer: Some(TxBaseConstants::OPEN_PAYER),
            ..Default::default()
        };
        let envelope = BaseTxEnvelope::Base(TxBaseSigned::new(
            tx,
            Bytes::from_static(&[0xab]),
            Bytes::from_static(&[0xcd]),
        ));
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["type"], "0x79");
        assert_eq!(json["chainId"], "0x2105");
        assert_eq!(json["nonce"], "0x7");
        assert_eq!(json["gas"], "0x5208");
        assert_eq!(json["validAfter"], "0x0");
        assert_eq!(json["validBefore"], "0x6553f100");
        assert_eq!(json["to"], serde_json::Value::Null);
        assert_eq!(json["value"], "0x0");
        assert_eq!(json["input"], "0x");
        assert_eq!(json["senderAuth"], "0xab");
        assert_eq!(json["payer"], "0x00");
        assert!(json.get("tx").is_none(), "the transaction fields are not nested");
        assert_eq!(serde_json::from_value::<BaseTxEnvelope>(json.clone()).unwrap(), envelope);

        let mut eip_names = json;
        let fields = eip_names.as_object_mut().unwrap();
        let nonce = fields.remove("nonce").unwrap();
        let gas = fields.remove("gas").unwrap();
        fields.insert("nonceSequence".into(), nonce);
        fields.insert("gasLimit".into(), gas);
        assert_eq!(serde_json::from_value::<BaseTxEnvelope>(eip_names).unwrap(), envelope);
    }

    #[test]
    fn size_counts_empty_call_phases() {
        let empty = TxBase { calls: Vec::new(), ..Default::default() };
        let with_phases = TxBase { calls: vec![Vec::new(); 1_024], ..Default::default() };

        assert!(
            with_phases.size() >= empty.size() + 1_024 * mem::size_of::<Vec<Call>>(),
            "empty phases must contribute outer Vec<slot> headers: empty={}, with_phases={}",
            empty.size(),
            with_phases.size()
        );
    }

    #[test]
    fn size_counts_call_data_heap() {
        let bare = TxBase {
            calls: vec![vec![Call { to: Address::ZERO, value: U256::ZERO, data: Bytes::new() }]],
            ..Default::default()
        };
        let with_data = TxBase {
            calls: vec![vec![Call {
                to: Address::ZERO,
                value: U256::ZERO,
                data: Bytes::from(vec![0xab; 4_096]),
            }]],
            ..Default::default()
        };

        assert!(
            with_data.size() >= bare.size() + 4_096,
            "calldata heap must be counted: bare={}, with_data={}",
            bare.size(),
            with_data.size()
        );
    }
}
