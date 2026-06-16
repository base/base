//! Unsigned [EIP-8130] Account Abstraction transaction body ([`TxEip8130`]).
//!
//! This module defines the unsigned payload of an EIP-8130 transaction. The
//! signed envelope (which wraps this type alongside the `sender_auth` and
//! `payer_auth` byte strings) lives in [`super::signed`].
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

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

use crate::transaction::eip8130::{
    account_changes::AccountChange, call::Call, constants::Eip8130Constants,
};

/// Unsigned body of an [EIP-8130] Account Abstraction transaction.
///
/// On the wire, the signed form (an [`super::Eip8130Signed`]) is
/// `EIP8130_TX_TYPE || rlp([...all fields..., sender_auth, payer_auth])`. The
/// unsigned struct here carries only the consensus fields; signature material
/// is held by [`super::Eip8130Signed`].
///
/// Field semantics follow the [EIP-8130] draft. Two fields are nullable on the
/// wire (encoded as a zero-length byte string when absent):
///
/// - [`Self::sender`]: `None` selects the EOA path (recovered from
///   `sender_auth` as a 65-byte ECDSA signature); `Some` selects the
///   configured-actor path with an explicit account address.
/// - [`Self::payer`]: `None` selects self-pay (the resolved sender pays);
///   `Some` selects sponsored pay (the payer address pays).
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TxEip8130 {
    /// EIP-155 chain ID this transaction is bound to.
    pub chain_id: ChainId,
    /// Explicit sender account address, or `None` for the EOA path.
    pub sender: Option<Address>,
    /// High 192 bits of the compound nonce; with `nonce_sequence` forms the
    /// per-account replay protection key.
    pub nonce_key: U256,
    /// Sequence number within the nonce key.
    pub nonce_sequence: u64,
    /// Unix-seconds expiry timestamp; `0` means no expiry.
    pub expiry: u64,
    /// Max priority fee per gas (tip) the sender is willing to pay.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_priority_fee_per_gas: u128,
    /// Max total fee per gas (base + tip cap) the sender is willing to pay.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_fee_per_gas: u128,
    /// Gas limit for the entire AA transaction execution.
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
    /// Optional explicit payer; `None` means the resolved sender pays gas.
    pub payer: Option<Address>,
}

impl TxEip8130 {
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

    fn decode_calls(buf: &mut &[u8]) -> alloy_rlp::Result<Vec<Vec<Call>>> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let started = buf.len();
        let mut phases = Vec::new();
        while started - buf.len() < header.payload_length {
            phases.push(Vec::<Call>::decode(buf)?);
        }
        let consumed = started - buf.len();
        if consumed != header.payload_length {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: consumed,
            });
        }
        Ok(phases)
    }

    /// Length of all RLP fields (no list header).
    pub fn rlp_encoded_fields_length(&self) -> usize {
        self.chain_id.length()
            + Self::address_opt_encoded_length(&self.sender)
            + self.nonce_key.length()
            + self.nonce_sequence.length()
            + self.expiry.length()
            + self.max_priority_fee_per_gas.length()
            + self.max_fee_per_gas.length()
            + self.gas_limit.length()
            + self.account_changes.length()
            + Self::calls_encoded_length(&self.calls)
            + self.metadata.length()
            + Self::address_opt_encoded_length(&self.payer)
    }

    /// Encodes the RLP fields (no list header) in canonical order.
    pub fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.chain_id.encode(out);
        Self::encode_address_opt(&self.sender, out);
        self.nonce_key.encode(out);
        self.nonce_sequence.encode(out);
        self.expiry.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
        self.gas_limit.encode(out);
        self.account_changes.encode(out);
        Self::encode_calls(&self.calls, out);
        self.metadata.encode(out);
        Self::encode_address_opt(&self.payer, out);
    }

    /// Decodes the RLP fields (no list header) in canonical order.
    pub fn rlp_decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self {
            chain_id: Decodable::decode(buf)?,
            sender: Self::decode_address_opt(buf)?,
            nonce_key: Decodable::decode(buf)?,
            nonce_sequence: Decodable::decode(buf)?,
            expiry: Decodable::decode(buf)?,
            max_priority_fee_per_gas: Decodable::decode(buf)?,
            max_fee_per_gas: Decodable::decode(buf)?,
            gas_limit: Decodable::decode(buf)?,
            account_changes: Decodable::decode(buf)?,
            calls: Self::decode_calls(buf)?,
            metadata: Decodable::decode(buf)?,
            payer: Self::decode_address_opt(buf)?,
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
            .unwrap_or_else(|err| panic!("invalid compact-encoded EIP-8130 tail: {err}"));
        assert!(header.list, "compact-encoded EIP-8130 tail must be an RLP list");
        let started = tail.len();

        let account_changes = Bytes::decode(&mut tail).unwrap_or_else(|err| {
            panic!("invalid compact-encoded EIP-8130 account changes: {err}")
        });
        let calls = Bytes::decode(&mut tail)
            .unwrap_or_else(|err| panic!("invalid compact-encoded EIP-8130 calls: {err}"));
        let metadata = Bytes::decode(&mut tail)
            .unwrap_or_else(|err| panic!("invalid compact-encoded EIP-8130 metadata: {err}"));
        let consumed = started - tail.len();
        assert_eq!(
            consumed, header.payload_length,
            "compact-encoded EIP-8130 tail length mismatch"
        );

        let mut account_changes_buf = account_changes.as_ref();
        let account_changes = Vec::<AccountChange>::decode(&mut account_changes_buf)
            .unwrap_or_else(|err| {
                panic!("invalid compact-encoded EIP-8130 account changes: {err}")
            });
        assert!(
            account_changes_buf.is_empty(),
            "compact-encoded EIP-8130 account changes left trailing bytes"
        );

        let mut calls_buf = calls.as_ref();
        let calls = Self::decode_calls(&mut calls_buf)
            .unwrap_or_else(|err| panic!("invalid compact-encoded EIP-8130 calls: {err}"));
        assert!(calls_buf.is_empty(), "compact-encoded EIP-8130 calls left trailing bytes");

        (account_changes, calls, metadata)
    }

    /// Signing-hash preimage for the sender, per [EIP-8130].
    ///
    /// `keccak256(EIP8130_TX_TYPE || rlp([...unsigned body fields...]))`.
    ///
    /// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
    pub fn sender_signature_hash(&self) -> B256 {
        let mut buf = Vec::with_capacity(self.rlp_encoded_length() + 1);
        buf.put_u8(Eip8130Constants::EIP8130_TX_TYPE);
        self.rlp_encode(&mut buf);
        keccak256(&buf)
    }

    /// Signing-hash preimage for the payer, per [EIP-8130].
    ///
    /// `keccak256(EIP8130_PAYER_TYPE || rlp([all body fields through `payer`]))`
    /// with the `sender` slot replaced by the recovered sender address. The
    /// payer commits to the full transaction body — including the `payer` slot
    /// itself — and only the `sender_auth` / `payer_auth` slots (which live in
    /// the signed wrapper) are excluded.
    ///
    /// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
    pub fn payer_signature_hash(&self, resolved_sender: Address) -> B256 {
        let with_resolved = Self { sender: Some(resolved_sender), ..self.clone() };
        let mut buf = Vec::with_capacity(with_resolved.rlp_encoded_length() + 1);
        buf.put_u8(Eip8130Constants::EIP8130_PAYER_TYPE);
        with_resolved.rlp_encode(&mut buf);
        keccak256(&buf)
    }

    /// In-memory size heuristic.
    pub fn size(&self) -> usize {
        mem::size_of::<ChainId>()
            + mem::size_of::<Option<Address>>()
            + mem::size_of::<U256>()
            + mem::size_of::<u64>()
            + mem::size_of::<u64>()
            + mem::size_of::<u128>()
            + mem::size_of::<u128>()
            + mem::size_of::<u64>()
            + self.account_changes.capacity() * mem::size_of::<AccountChange>()
            + self.calls.iter().map(|p| p.capacity() * mem::size_of::<Call>()).sum::<usize>()
            + self.metadata.len()
            + mem::size_of::<Option<Address>>()
    }
}

impl Encodable for TxEip8130 {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_encode(out);
    }

    fn length(&self) -> usize {
        self.rlp_encoded_length()
    }
}

impl Decodable for TxEip8130 {
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
struct CompactTxEip8130Head {
    chain_id: ChainId,
    sender: Option<Address>,
    nonce_key: U256,
    nonce_sequence: u64,
    expiry: u64,
    max_priority_fee_per_gas: u128,
    max_fee_per_gas: u128,
    gas_limit: u64,
    payer: Option<Address>,
    tail_len: u64,
}

#[cfg(feature = "reth")]
impl Compact for TxEip8130 {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let tail = self.compact_tail();
        let head = CompactTxEip8130Head {
            chain_id: self.chain_id,
            sender: self.sender,
            nonce_key: self.nonce_key,
            nonce_sequence: self.nonce_sequence,
            expiry: self.expiry,
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
        let (head, buf) = CompactTxEip8130Head::from_compact(buf, len);
        let CompactTxEip8130Head {
            chain_id,
            sender,
            nonce_key,
            nonce_sequence,
            expiry,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            gas_limit,
            payer,
            tail_len,
        } = head;

        let tail_len: usize = tail_len.try_into().expect("EIP-8130 compact tail length overflow");
        assert!(
            buf.len() >= tail_len,
            "compact-encoded EIP-8130 tail shorter than declared length"
        );
        let (tail, buf) = buf.split_at(tail_len);
        let (account_changes, calls, metadata) = Self::decode_compact_tail(tail);

        (
            Self {
                chain_id,
                sender,
                nonce_key,
                nonce_sequence,
                expiry,
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

impl Typed2718 for TxEip8130 {
    fn ty(&self) -> u8 {
        Eip8130Constants::EIP8130_TX_TYPE
    }
}

impl IsTyped2718 for TxEip8130 {
    fn is_type(ty: u8) -> bool {
        ty == Eip8130Constants::EIP8130_TX_TYPE
    }
}

impl InMemorySize for TxEip8130 {
    fn size(&self) -> usize {
        Self::size(self)
    }
}

impl Transaction for TxEip8130 {
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

impl SignableTransaction<Signature> for TxEip8130 {
    fn set_chain_id(&mut self, chain_id: ChainId) {
        self.chain_id = chain_id;
    }

    fn encode_for_signing(&self, out: &mut dyn BufMut) {
        out.put_u8(Eip8130Constants::EIP8130_TX_TYPE);
        self.rlp_encode(out);
    }

    fn payload_len_for_signature(&self) -> usize {
        1 + self.rlp_encoded_length()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, bytes};
    #[cfg(feature = "reth")]
    use reth_codecs::Compact;

    use super::*;
    use crate::transaction::eip8130::account_changes::Delegation;

    fn sample_tx() -> TxEip8130 {
        TxEip8130 {
            chain_id: 8453,
            sender: Some(address!("0x00000000000000000000000000000000000000aa")),
            nonce_key: U256::from(0x1234u64),
            nonce_sequence: 7,
            expiry: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 200_000,
            account_changes: vec![AccountChange::Delegation(Delegation {
                target: address!("0x00000000000000000000000000000000000000bb"),
            })],
            calls: vec![vec![Call {
                to: address!("0x00000000000000000000000000000000000000cc"),
                data: bytes!("deadbeef"),
            }]],
            metadata: bytes!("c0ffee"),
            payer: None,
        }
    }

    #[test]
    fn rlp_roundtrip_full() {
        let tx = sample_tx();
        let mut buf = Vec::new();
        tx.rlp_encode(&mut buf);
        assert_eq!(buf.len(), tx.rlp_encoded_length());
        let decoded = TxEip8130::rlp_decode_fields(&mut {
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
        let decoded = TxEip8130::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    #[cfg(feature = "reth")]
    #[test]
    fn compact_roundtrip() {
        let tx = sample_tx();
        let mut buf = Vec::new();
        let len = tx.to_compact(&mut buf);
        let (decoded, remaining) = TxEip8130::from_compact(&buf, len);

        assert_eq!(decoded, tx);
        assert!(remaining.is_empty());
    }

    #[cfg(feature = "reth")]
    #[test]
    fn compact_roundtrip_eoa_sender_with_explicit_payer() {
        let tx = TxEip8130 {
            sender: None,
            payer: Some(address!("0x00000000000000000000000000000000000000dd")),
            ..sample_tx()
        };
        let mut buf = Vec::new();
        let len = tx.to_compact(&mut buf);
        let (decoded, remaining) = TxEip8130::from_compact(&buf, len);

        assert_eq!(decoded, tx);
        assert!(remaining.is_empty());
    }

    #[cfg(feature = "reth")]
    #[test]
    fn compact_decode_preserves_trailing_bytes() {
        let tx = sample_tx();
        let trailing = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut buf = Vec::new();
        let _ = tx.to_compact(&mut buf);
        buf.extend_from_slice(&trailing);

        let (decoded, remaining) = TxEip8130::from_compact(&buf, buf.len());

        assert_eq!(decoded, tx);
        assert_eq!(remaining, trailing);
    }

    #[test]
    fn rlp_roundtrip_minimal_empty() {
        let tx = TxEip8130::default();
        let mut buf = Vec::new();
        tx.encode(&mut buf);
        let decoded = TxEip8130::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn address_opt_roundtrip_none() {
        let mut buf = Vec::new();
        TxEip8130::encode_address_opt(&None, &mut buf);
        assert_eq!(buf, vec![0x80]);
        let decoded = TxEip8130::decode_address_opt(&mut buf.as_slice()).unwrap();
        assert_eq!(decoded, None);
    }

    #[test]
    fn address_opt_roundtrip_some() {
        let addr = address!("0x00000000000000000000000000000000000000ff");
        let mut buf = Vec::new();
        TxEip8130::encode_address_opt(&Some(addr), &mut buf);
        let decoded = TxEip8130::decode_address_opt(&mut buf.as_slice()).unwrap();
        assert_eq!(decoded, Some(addr));
    }

    #[test]
    fn address_opt_rejects_wrong_length() {
        let mut buf = Vec::new();
        Bytes::copy_from_slice(&[0u8; 19]).encode(&mut buf);
        let res = TxEip8130::decode_address_opt(&mut buf.as_slice());
        assert!(res.is_err());
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
    fn signing_hashes_use_prefix_bytes() {
        let tx = sample_tx();
        let h = tx.sender_signature_hash();
        assert_ne!(h, B256::ZERO);
    }

    #[test]
    fn ty_byte_matches_constant() {
        assert_eq!(sample_tx().ty(), Eip8130Constants::EIP8130_TX_TYPE);
        assert!(<TxEip8130 as IsTyped2718>::is_type(Eip8130Constants::EIP8130_TX_TYPE));
        assert!(!<TxEip8130 as IsTyped2718>::is_type(0x00));
    }

    #[test]
    fn nested_calls_roundtrip() {
        let tx = TxEip8130 {
            chain_id: 1,
            calls: vec![
                vec![Call { to: Address::ZERO, data: bytes!("01") }],
                vec![],
                vec![
                    Call { to: Address::ZERO, data: bytes!("02") },
                    Call { to: Address::ZERO, data: bytes!("03") },
                ],
            ],
            ..Default::default()
        };
        let mut buf = Vec::new();
        tx.encode(&mut buf);
        let decoded = TxEip8130::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn account_change_roundtrip_in_tx() {
        let tx = TxEip8130 {
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
        let decoded = TxEip8130::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx.account_changes, decoded.account_changes);
    }

    #[test]
    fn payer_signature_hash_uses_substituted_sender() {
        let mut tx = sample_tx();
        tx.sender = None;
        let resolved = address!("0x00000000000000000000000000000000000000dd");
        let payer_hash_v1 = tx.payer_signature_hash(resolved);

        let tx2 = TxEip8130 { sender: Some(resolved), ..tx };
        let mut buf = Vec::with_capacity(tx2.rlp_encoded_length() + 1);
        buf.put_u8(Eip8130Constants::EIP8130_PAYER_TYPE);
        tx2.rlp_encode(&mut buf);
        let payer_hash_v2 = keccak256(&buf);
        assert_eq!(payer_hash_v1, payer_hash_v2);
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
        let sponsored = TxEip8130 {
            payer: Some(address!("0x00000000000000000000000000000000000000ee")),
            ..self_pay.clone()
        };

        assert_ne!(
            self_pay.payer_signature_hash(resolved),
            sponsored.payer_signature_hash(resolved)
        );
        assert_ne!(self_pay.sender_signature_hash(), sponsored.sender_signature_hash());
    }

    /// Proves that EIP-8130 transactions can be deserialized through the
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
    fn eip8130_deserializes_through_envelope() {
        use crate::BaseTxEnvelope;

        let json = serde_json::json!({
            "type": "0x7b",
            "tx": {
                "chainId": 1,
                "sender": null,
                "nonceKey": "0x0",
                "nonceSequence": 1,
                "expiry": 0,
                "maxPriorityFeePerGas": "0x3b9aca00",
                "maxFeePerGas": "0x3b9aca00",
                "gasLimit": 21000,
                "accountChanges": [],
                "calls": [],
                "metadata": "0x",
                "payer": null
            },
            "senderAuth": "0x",
            "payerAuth": "0x"
        });

        let result = serde_json::from_value::<BaseTxEnvelope>(json);
        assert!(
            result.is_ok(),
            "EIP-8130 envelope deserialization failed: {:#}",
            result.unwrap_err()
        );

        let envelope = result.unwrap();
        assert!(matches!(envelope, BaseTxEnvelope::Eip8130(_)));
    }
}
