//! The EIP-8130 AA transaction type.

use alloc::vec::Vec;

use alloy_consensus::{Sealable, Transaction, Typed2718};
use alloy_eips::eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718, IsTyped2718};
use alloy_primitives::{Address, B256, Bytes, ChainId, TxKind, U256, keccak256};
use alloy_rlp::{BufMut, Decodable, Encodable, Header, length_of_length};

use super::{
    AccountChangeEntry, Call, auth_verifier_kind, constants::AA_TX_TYPE_ID, parse_delegate_auth,
};

/// An EIP-8130 account-abstracted transaction.
///
/// AA transactions have embedded authentication (`sender_auth`, `payer_auth`) and
/// use phased call batching instead of a single `to` + `input`.
///
/// RLP: `[chain_id, from, nonce_key, nonce_sequence, expiry,
///        max_priority_fee_per_gas, max_fee_per_gas, gas_limit,
///        account_changes, calls, payer,
///        sender_auth, payer_auth]`
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct TxEip8130 {
    /// Chain ID this transaction targets.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub chain_id: u64,
    /// Sender address. `None` means the sender is derived via ecrecover.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub from: Option<Address>,
    /// 2D nonce channel selector (uint256).
    pub nonce_key: U256,
    /// Sequence number within the nonce channel.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub nonce_sequence: u64,
    /// Block timestamp after which this transaction is invalid. `0` = no expiry.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub expiry: u64,
    /// EIP-1559 priority fee.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_priority_fee_per_gas: u128,
    /// EIP-1559 max fee.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_fee_per_gas: u128,
    /// Execution gas budget (excludes intrinsic cost).
    #[cfg_attr(
        feature = "serde",
        serde(with = "alloy_serde::quantity", rename = "gas", alias = "gasLimit")
    )]
    pub gas_limit: u64,
    /// Account creation and/or configuration change entries.
    pub account_changes: Vec<AccountChangeEntry>,
    /// Phased call batches. Each inner `Vec` is one atomic phase.
    pub calls: Vec<Vec<Call>>,
    /// Payer address. `None` means the sender pays for gas.
    pub payer: Option<Address>,
    /// Sender authentication data.
    pub sender_auth: Bytes,
    /// Payer authentication data (empty if self-pay).
    pub payer_auth: Bytes,
}

impl TxEip8130 {
    fn auth_uses_delegate_staticcall(auth: &[u8]) -> bool {
        let Some(parsed) = parse_delegate_auth(auth) else {
            return false;
        };
        let nested_verifier = parsed.nested_verifier;
        if nested_verifier == Address::ZERO {
            return false;
        }
        // Delegate envelope:
        // DELEGATE(20) || delegate_account(20) || nested_auth(verifier(20) || ...)
        // Only nested custom verifiers require STATICCALL.
        auth_verifier_kind(parsed.nested_auth).is_some_and(|kind| kind.is_custom())
    }

    /// Returns `true` if this is an EOA-mode transaction (sender derived via ecrecover).
    pub fn is_eoa(&self) -> bool {
        self.from.is_none()
    }

    /// Returns `true` if the sender pays for gas (no external payer).
    pub fn is_self_pay(&self) -> bool {
        self.payer.is_none()
    }

    /// Returns `true` if sender authentication uses a custom verifier.
    pub fn sender_has_custom_verifier(&self) -> bool {
        !self.is_eoa()
            && (auth_verifier_kind(&self.sender_auth).is_some_and(|verifier| verifier.is_custom())
                || Self::auth_uses_delegate_staticcall(&self.sender_auth))
    }

    /// Returns `true` if payer authentication uses a custom verifier.
    pub fn payer_has_custom_verifier(&self) -> bool {
        !self.is_self_pay()
            && (auth_verifier_kind(&self.payer_auth).is_some_and(|verifier| verifier.is_custom())
                || Self::auth_uses_delegate_staticcall(&self.payer_auth))
    }

    /// Returns `true` if any config-change authorizer uses a custom verifier.
    pub fn authorizer_has_custom_verifier(&self) -> bool {
        self.account_changes.iter().any(|entry| {
            matches!(entry, AccountChangeEntry::ConfigChange(cc)
                if auth_verifier_kind(&cc.authorizer_auth)
                    .is_some_and(|verifier| verifier.is_custom())
                    || Self::auth_uses_delegate_staticcall(&cc.authorizer_auth))
        })
    }

    /// Returns `true` if any auth path uses a custom verifier.
    pub fn has_custom_verifier(&self) -> bool {
        self.sender_has_custom_verifier()
            || self.payer_has_custom_verifier()
            || self.authorizer_has_custom_verifier()
    }

    /// Computes and returns the transaction hash (EIP-2718 envelope hash).
    pub fn tx_hash(&self) -> B256 {
        let mut buf = Vec::with_capacity(self.encode_2718_len());
        self.encode_2718(&mut buf);
        keccak256(&buf)
    }

    /// Returns the sender address as specified in the `from` field.
    ///
    /// For EOA-mode transactions (`from` is empty), the actual sender
    /// must be recovered from `sender_auth` via ecrecover during validation.
    /// This method returns `Address::ZERO` as a placeholder for EOA-mode txs.
    pub fn effective_sender(&self) -> Address {
        self.from.unwrap_or(Address::ZERO)
    }

    /// Returns the effective payer address (sender if self-pay).
    pub fn effective_payer(&self) -> Address {
        if self.is_self_pay() {
            self.effective_sender()
        } else {
            self.payer.unwrap_or(Address::ZERO)
        }
    }

    /// Encodes the inner fields into an RLP list payload (no outer header).
    fn encode_fields(&self, out: &mut dyn BufMut) {
        self.chain_id.encode(out);
        encode_optional_address(&self.from, out);
        self.nonce_key.encode(out);
        self.nonce_sequence.encode(out);
        self.expiry.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
        self.gas_limit.encode(out);
        encode_list(&self.account_changes, out);
        encode_nested_calls(&self.calls, out);
        encode_optional_address(&self.payer, out);
        self.sender_auth.encode(out);
        self.payer_auth.encode(out);
    }

    /// Computes the combined length of all encoded fields (the RLP list payload length).
    fn fields_len(&self) -> usize {
        self.chain_id.length()
            + optional_address_len(&self.from)
            + self.nonce_key.length()
            + self.nonce_sequence.length()
            + self.expiry.length()
            + self.max_priority_fee_per_gas.length()
            + self.max_fee_per_gas.length()
            + self.gas_limit.length()
            + list_len(&self.account_changes)
            + nested_calls_len(&self.calls)
            + optional_address_len(&self.payer)
            + self.sender_auth.length()
            + self.payer_auth.length()
    }

    /// Decodes the inner fields from an RLP buffer (list header already consumed).
    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self {
            chain_id: Decodable::decode(buf)?,
            from: decode_optional_address(buf)?,
            nonce_key: Decodable::decode(buf)?,
            nonce_sequence: Decodable::decode(buf)?,
            expiry: Decodable::decode(buf)?,
            max_priority_fee_per_gas: Decodable::decode(buf)?,
            max_fee_per_gas: Decodable::decode(buf)?,
            gas_limit: Decodable::decode(buf)?,
            account_changes: Decodable::decode(buf)?,
            calls: decode_nested_calls(buf)?,
            payer: decode_optional_address(buf)?,
            sender_auth: Decodable::decode(buf)?,
            payer_auth: Decodable::decode(buf)?,
        })
    }

    /// Returns the RLP-encoded fields length (payload only, no outer list header).
    pub fn rlp_encoded_fields_length(&self) -> usize {
        self.fields_len()
    }

    /// RLP-encodes the fields (payload only, no outer list header).
    pub fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.encode_fields(out);
    }

    fn rlp_header(&self) -> Header {
        Header { list: true, payload_length: self.rlp_encoded_fields_length() }
    }

    /// RLP-encodes the transaction (header + fields).
    pub fn rlp_encode(&self, out: &mut dyn BufMut) {
        self.rlp_header().encode(out);
        self.rlp_encode_fields(out);
    }

    /// Returns the RLP-encoded length (header + payload).
    pub fn rlp_encoded_length(&self) -> usize {
        self.rlp_header().length_with_payload()
    }

    /// Returns the EIP-2718 encoded length (1-byte type flag + RLP).
    pub fn eip2718_encoded_length(&self) -> usize {
        self.rlp_encoded_length() + 1
    }

    fn network_header(&self) -> Header {
        Header { list: false, payload_length: self.eip2718_encoded_length() }
    }

    /// Returns the network-encoded length (outer RLP header + EIP-2718 encoding).
    pub fn network_encoded_length(&self) -> usize {
        self.network_header().length_with_payload()
    }

    /// Network-encodes the transaction.
    pub fn network_encode(&self, out: &mut dyn BufMut) {
        self.network_header().encode(out);
        self.encode_2718(out);
    }

    /// Decodes from an RLP list (header + fields).
    pub fn rlp_decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let remaining = buf.len();

        let this = Self::rlp_decode_fields(buf)?;

        if buf.len() + header.payload_length != remaining {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        Ok(this)
    }

    /// Encodes the fields that go into the **sender** signature hash.
    ///
    /// `keccak256(AA_TX_TYPE || rlp([chain_id, from, nonce_key, nonce_sequence, expiry,
    ///   max_priority_fee_per_gas, max_fee_per_gas, gas_limit,
    ///   account_changes, calls, payer]))`
    pub fn encode_for_sender_signing(&self, out: &mut dyn BufMut) {
        let payload_len = self.chain_id.length()
            + optional_address_len(&self.from)
            + self.nonce_key.length()
            + self.nonce_sequence.length()
            + self.expiry.length()
            + self.max_priority_fee_per_gas.length()
            + self.max_fee_per_gas.length()
            + self.gas_limit.length()
            + list_len(&self.account_changes)
            + nested_calls_len(&self.calls)
            + optional_address_len(&self.payer);

        out.put_u8(AA_TX_TYPE_ID);
        Header { list: true, payload_length: payload_len }.encode(out);
        self.chain_id.encode(out);
        encode_optional_address(&self.from, out);
        self.nonce_key.encode(out);
        self.nonce_sequence.encode(out);
        self.expiry.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
        self.gas_limit.encode(out);
        encode_list(&self.account_changes, out);
        encode_nested_calls(&self.calls, out);
        encode_optional_address(&self.payer, out);
    }

    /// Encodes the fields that go into the **payer** signature hash.
    ///
    /// `keccak256(AA_PAYER_TYPE || rlp([chain_id, from, nonce_key, nonce_sequence, expiry,
    ///   max_priority_fee_per_gas, max_fee_per_gas, gas_limit,
    ///   account_changes, calls]))`
    pub fn encode_for_payer_signing(&self, out: &mut dyn BufMut) {
        let payload_len = self.chain_id.length()
            + optional_address_len(&self.from)
            + self.nonce_key.length()
            + self.nonce_sequence.length()
            + self.expiry.length()
            + self.max_priority_fee_per_gas.length()
            + self.max_fee_per_gas.length()
            + self.gas_limit.length()
            + list_len(&self.account_changes)
            + nested_calls_len(&self.calls);

        out.put_u8(super::constants::AA_PAYER_TYPE);
        Header { list: true, payload_length: payload_len }.encode(out);
        self.chain_id.encode(out);
        encode_optional_address(&self.from, out);
        self.nonce_key.encode(out);
        self.nonce_sequence.encode(out);
        self.expiry.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
        self.gas_limit.encode(out);
        encode_list(&self.account_changes, out);
        encode_nested_calls(&self.calls, out);
    }
}

// ---------------------------------------------------------------------------
// alloy_rlp::Encodable / Decodable
// ---------------------------------------------------------------------------

impl Encodable for TxEip8130 {
    fn encode(&self, out: &mut dyn BufMut) {
        let payload = self.fields_len();
        Header { list: true, payload_length: payload }.encode(out);
        self.encode_fields(out);
    }

    fn length(&self) -> usize {
        let payload = self.fields_len();
        payload + length_of_length(payload)
    }
}

impl Decodable for TxEip8130 {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::rlp_decode(buf)
    }
}

// ---------------------------------------------------------------------------
// Sealable
// ---------------------------------------------------------------------------

impl Sealable for TxEip8130 {
    fn hash_slow(&self) -> B256 {
        self.tx_hash()
    }
}

// ---------------------------------------------------------------------------
// EIP-2718
// ---------------------------------------------------------------------------

impl Typed2718 for TxEip8130 {
    fn ty(&self) -> u8 {
        AA_TX_TYPE_ID
    }
}

impl IsTyped2718 for TxEip8130 {
    fn is_type(ty: u8) -> bool {
        ty == AA_TX_TYPE_ID
    }
}

impl Encodable2718 for TxEip8130 {
    fn type_flag(&self) -> Option<u8> {
        Some(AA_TX_TYPE_ID)
    }

    fn encode_2718_len(&self) -> usize {
        1 + self.length()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        out.put_u8(AA_TX_TYPE_ID);
        self.encode(out);
    }
}

impl Decodable2718 for TxEip8130 {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        if ty != AA_TX_TYPE_ID {
            return Err(Eip2718Error::UnexpectedType(ty));
        }
        Self::rlp_decode(buf).map_err(Into::into)
    }

    fn fallback_decode(_buf: &mut &[u8]) -> Eip2718Result<Self> {
        Err(Eip2718Error::UnexpectedType(0))
    }
}

// ---------------------------------------------------------------------------
// Transaction trait
// ---------------------------------------------------------------------------

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
        base_fee.map_or(self.max_fee_per_gas, |base_fee| {
            let tip = self.max_fee_per_gas.saturating_sub(base_fee as u128);
            let tip = tip.min(self.max_priority_fee_per_gas);
            base_fee as u128 + tip
        })
    }

    fn is_dynamic_fee(&self) -> bool {
        true
    }

    fn is_create(&self) -> bool {
        false
    }

    fn kind(&self) -> TxKind {
        TxKind::Call(self.effective_sender())
    }

    fn value(&self) -> U256 {
        U256::ZERO
    }

    fn input(&self) -> &Bytes {
        static EMPTY: Bytes = Bytes::new();
        &EMPTY
    }

    fn access_list(&self) -> Option<&alloy_eips::eip2930::AccessList> {
        None
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        None
    }

    fn authorization_list(&self) -> Option<&[alloy_eips::eip7702::SignedAuthorization]> {
        None
    }

    fn effective_tip_per_gas(&self, base_fee: u64) -> Option<u128> {
        let max_fee = self.max_fee_per_gas;
        let base = base_fee as u128;
        if max_fee < base {
            return None;
        }
        Some((max_fee - base).min(self.max_priority_fee_per_gas))
    }
}

// ---------------------------------------------------------------------------
// RLP helpers for nested structures
// ---------------------------------------------------------------------------

fn encode_list<T: Encodable>(items: &[T], out: &mut dyn BufMut) {
    let payload_len: usize = items.iter().map(Encodable::length).sum();
    Header { list: true, payload_length: payload_len }.encode(out);
    for item in items {
        item.encode(out);
    }
}

fn encode_optional_address(addr: &Option<Address>, out: &mut dyn BufMut) {
    match addr {
        Some(address) => address.encode(out),
        None => out.put_u8(0x80),
    }
}

fn optional_address_len(addr: &Option<Address>) -> usize {
    match addr {
        Some(address) => address.length(),
        None => 1,
    }
}

fn decode_optional_address(buf: &mut &[u8]) -> alloy_rlp::Result<Option<Address>> {
    let header = Header::decode(buf)?;
    if header.list {
        return Err(alloy_rlp::Error::UnexpectedList);
    }
    match header.payload_length {
        0 => Ok(None),
        20 => {
            if buf.len() < 20 {
                return Err(alloy_rlp::Error::UnexpectedLength);
            }
            let address = Address::from_slice(&buf[..20]);
            *buf = &buf[20..];
            Ok(Some(address))
        }
        _ => Err(alloy_rlp::Error::UnexpectedLength),
    }
}

fn list_len<T: Encodable>(items: &[T]) -> usize {
    let payload_len: usize = items.iter().map(Encodable::length).sum();
    payload_len + length_of_length(payload_len)
}

fn encode_nested_calls(phases: &[Vec<Call>], out: &mut dyn BufMut) {
    let payload_len: usize = phases.iter().map(|phase| list_len(phase.as_slice())).sum();
    Header { list: true, payload_length: payload_len }.encode(out);
    for phase in phases {
        encode_list(phase, out);
    }
}

fn nested_calls_len(phases: &[Vec<Call>]) -> usize {
    let payload_len: usize = phases.iter().map(|phase| list_len(phase.as_slice())).sum();
    payload_len + length_of_length(payload_len)
}

fn decode_nested_calls(buf: &mut &[u8]) -> alloy_rlp::Result<Vec<Vec<Call>>> {
    let outer = Header::decode(buf)?;
    if !outer.list {
        return Err(alloy_rlp::Error::UnexpectedString);
    }
    let outer_end = buf.len() - outer.payload_length;
    let mut phases = Vec::new();
    while buf.len() > outer_end {
        let inner = Header::decode(buf)?;
        if !inner.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let inner_end = buf.len() - inner.payload_length;
        let mut calls = Vec::new();
        while buf.len() > inner_end {
            calls.push(Call::decode(buf)?);
        }
        phases.push(calls);
    }
    Ok(phases)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use super::*;

    fn auth_blob(verifier: Address) -> Bytes {
        let mut blob = verifier.as_slice().to_vec();
        blob.push(0x01);
        Bytes::from(blob)
    }

    fn sample_tx() -> TxEip8130 {
        TxEip8130 {
            chain_id: 8453,
            from: Some(Address::repeat_byte(0x01)),
            nonce_key: U256::from(0u64),
            nonce_sequence: 42,
            expiry: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 10_000_000_000,
            gas_limit: 100_000,
            account_changes: vec![],
            calls: vec![vec![Call {
                to: Address::repeat_byte(0xBB),
                data: Bytes::from_static(&[0xDE, 0xAD]),
            }]],
            payer: None,
            sender_auth: Bytes::from_static(&[0xFF; 65]),
            payer_auth: Bytes::new(),
        }
    }

    #[test]
    fn rlp_round_trip() {
        let tx = sample_tx();
        let mut buf = Vec::new();
        tx.encode(&mut buf);
        let decoded = TxEip8130::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn eip2718_round_trip() {
        let tx = sample_tx();
        let mut buf = Vec::new();
        tx.encode_2718(&mut buf);
        assert_eq!(buf[0], AA_TX_TYPE_ID);
        let decoded = TxEip8130::decode_2718(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
        assert_eq!(buf.len(), tx.encode_2718_len());
    }

    #[test]
    fn tx_trait_getters() {
        let tx = sample_tx();
        assert_eq!(Transaction::chain_id(&tx), Some(8453));
        assert_eq!(tx.nonce(), 42);
        assert_eq!(tx.gas_limit(), 100_000);
        assert!(tx.gas_price().is_none());
        assert_eq!(tx.max_fee_per_gas(), 10_000_000_000);
        assert_eq!(tx.max_priority_fee_per_gas(), Some(1_000_000_000));
        assert!(tx.is_dynamic_fee());
        assert_eq!(tx.value(), U256::ZERO);
        assert_eq!(tx.ty(), AA_TX_TYPE_ID);
    }

    #[test]
    fn sender_and_payer_signing_differ() {
        let tx = sample_tx();
        let mut sender_buf = Vec::new();
        tx.encode_for_sender_signing(&mut sender_buf);
        let mut payer_buf = Vec::new();
        tx.encode_for_payer_signing(&mut payer_buf);
        assert_ne!(
            keccak256(&sender_buf),
            keccak256(&payer_buf),
            "sender and payer signature hashes must differ for domain separation"
        );
    }

    #[test]
    fn is_eoa_and_is_self_pay() {
        let mut tx = sample_tx();
        assert!(!tx.is_eoa());
        assert!(tx.is_self_pay());

        tx.from = None;
        assert!(tx.is_eoa());

        tx.payer = Some(Address::repeat_byte(0xCC));
        assert!(!tx.is_self_pay());
        assert_eq!(tx.effective_payer(), Address::repeat_byte(0xCC));
    }

    #[test]
    fn custom_verifier_detection_paths() {
        let custom = Address::repeat_byte(0xAB);
        let mut tx = sample_tx();
        tx.sender_auth = auth_blob(custom);
        assert!(tx.sender_has_custom_verifier());
        assert!(tx.has_custom_verifier());

        tx.sender_auth = auth_blob(super::super::K1_VERIFIER_ADDRESS);
        tx.payer = Some(Address::repeat_byte(0x22));
        tx.payer_auth = auth_blob(custom);
        assert!(tx.payer_has_custom_verifier());
        assert!(tx.has_custom_verifier());

        tx.payer_auth = auth_blob(super::super::P256_RAW_VERIFIER_ADDRESS);
        tx.account_changes =
            vec![AccountChangeEntry::ConfigChange(super::super::ConfigChangeEntry {
                chain_id: 0,
                sequence: 0,
                owner_changes: vec![],
                authorizer_auth: auth_blob(custom),
            })];
        assert!(tx.authorizer_has_custom_verifier());
        assert!(tx.has_custom_verifier());

        let delegate_target = Address::repeat_byte(0x42);
        let mut delegate_authorizer = Vec::new();
        delegate_authorizer.extend_from_slice(super::super::DELEGATE_VERIFIER_ADDRESS.as_slice());
        delegate_authorizer.extend_from_slice(delegate_target.as_slice());
        delegate_authorizer.extend_from_slice(custom.as_slice());
        delegate_authorizer.extend_from_slice(&[0xDD; 12]);
        tx.account_changes =
            vec![AccountChangeEntry::ConfigChange(super::super::ConfigChangeEntry {
                chain_id: 0,
                sequence: 0,
                owner_changes: vec![],
                authorizer_auth: Bytes::from(delegate_authorizer),
            })];
        assert!(tx.authorizer_has_custom_verifier());
        assert!(tx.has_custom_verifier());

        // Delegate-wrapped sender auth using STATICCALL path:
        // DELEGATE || delegate_account || CUSTOM || custom_data
        let mut delegate_wrapped = Vec::new();
        delegate_wrapped.extend_from_slice(super::super::DELEGATE_VERIFIER_ADDRESS.as_slice());
        delegate_wrapped.extend_from_slice(delegate_target.as_slice());
        delegate_wrapped.extend_from_slice(custom.as_slice());
        delegate_wrapped.extend_from_slice(&[0xEE; 12]);
        tx.sender_auth = Bytes::from(delegate_wrapped);
        tx.payer = None;
        tx.payer_auth = Bytes::new();
        tx.account_changes.clear();
        assert!(tx.sender_has_custom_verifier());
        assert!(tx.has_custom_verifier());

        // Delegate-wrapped sender auth with native nested verifier stays native:
        // DELEGATE || delegate_account || P256_RAW || p256_data
        let mut delegate_native = Vec::new();
        delegate_native.extend_from_slice(super::super::DELEGATE_VERIFIER_ADDRESS.as_slice());
        delegate_native.extend_from_slice(delegate_target.as_slice());
        delegate_native.extend_from_slice(super::super::P256_RAW_VERIFIER_ADDRESS.as_slice());
        delegate_native.extend_from_slice(&[0xAA; 128]);
        tx.sender_auth = Bytes::from(delegate_native);
        assert!(!tx.sender_has_custom_verifier());
    }

    #[test]
    fn empty_tx_rlp_round_trip() {
        let tx = TxEip8130::default();
        let mut buf = Vec::new();
        tx.encode(&mut buf);
        let decoded = TxEip8130::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn optional_address_helpers_round_trip() {
        let mut empty_buf = Vec::new();
        encode_optional_address(&None, &mut empty_buf);
        assert_eq!(empty_buf, vec![0x80]);
        let empty_decoded = decode_optional_address(&mut empty_buf.as_slice()).unwrap();
        assert_eq!(empty_decoded, None);

        let address = Address::repeat_byte(0xAB);
        let mut address_buf = Vec::new();
        encode_optional_address(&Some(address), &mut address_buf);
        assert_eq!(optional_address_len(&Some(address)), address_buf.len());
        let address_decoded = decode_optional_address(&mut address_buf.as_slice()).unwrap();
        assert_eq!(address_decoded, Some(address));
    }

    #[test]
    fn multi_phase_calls_round_trip() {
        let tx = TxEip8130 {
            calls: vec![
                vec![
                    Call { to: Address::repeat_byte(1), data: Bytes::from_static(&[0x01]) },
                    Call { to: Address::repeat_byte(2), data: Bytes::from_static(&[0x02]) },
                ],
                vec![Call { to: Address::repeat_byte(3), data: Bytes::from_static(&[0x03]) }],
            ],
            ..Default::default()
        };
        let mut buf = Vec::new();
        tx.encode(&mut buf);
        let decoded = TxEip8130::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx.calls.len(), 2);
        assert_eq!(decoded.calls.len(), 2);
        assert_eq!(decoded.calls[0].len(), 2);
        assert_eq!(decoded.calls[1].len(), 1);
    }
}
