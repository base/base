//! [EIP-8130] `account_changes` entry types.
//!
//! An [`AccountChange`] is a tagged-union entry inside `TxAa8130::account_changes`.
//! On the wire, each entry is encoded as `type_byte || rlp([entry_fields...])`.
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes};
use alloy_rlp::{
    Buf, BufMut, Decodable, Encodable, Header, RlpDecodable, RlpEncodable, length_of_length,
};

use crate::transaction::aa8130::constants::Aa8130Constants;

/// Bitmask describing the contexts in which an owner is valid.
///
/// On the wire, `Scope` is encoded as a single RLP-encoded byte (matching the
/// EIP-8130 `uint8` spec), not as a one-element list. The derived RLP impls
/// from `alloy_rlp` would wrap the inner byte in a list header, so the
/// `Encodable`/`Decodable` impls are written by hand.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Scope(pub u8);

impl Encodable for Scope {
    fn encode(&self, out: &mut dyn BufMut) {
        self.0.encode(out);
    }

    fn length(&self) -> usize {
        self.0.length()
    }
}

impl Decodable for Scope {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        u8::decode(buf).map(Self)
    }
}

impl Scope {
    /// Unrestricted scope (owner valid in all contexts).
    pub const UNRESTRICTED: Self = Self(Aa8130Constants::SCOPE_UNRESTRICTED);

    /// Returns the raw bitmask.
    pub const fn bits(&self) -> u8 {
        self.0
    }

    /// Returns true if the scope grants the `SCOPE_SIGNATURE` context.
    pub const fn has_signature(&self) -> bool {
        self.0 & Aa8130Constants::SCOPE_SIGNATURE != 0
    }

    /// Returns true if the scope grants the `SCOPE_SENDER` context.
    pub const fn has_sender(&self) -> bool {
        self.0 & Aa8130Constants::SCOPE_SENDER != 0
    }

    /// Returns true if the scope grants the `SCOPE_PAYER` context.
    pub const fn has_payer(&self) -> bool {
        self.0 & Aa8130Constants::SCOPE_PAYER != 0
    }

    /// Returns true if the scope grants the `SCOPE_CONFIG` context.
    pub const fn has_config(&self) -> bool {
        self.0 & Aa8130Constants::SCOPE_CONFIG != 0
    }
}

/// Initial owner installed on a newly-created account.
#[derive(Debug, Clone, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct InitialOwner {
    /// Address of the verifier contract (e.g. an ERC-1271 verifier).
    pub verifier: Address,
    /// Owner identifier passed to the verifier.
    pub owner_id: B256,
    /// Scope bitmask granted to this owner.
    pub scope: Scope,
}

/// Operation performed by an [`OwnerChange`] inside a [`ConfigChange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum OwnerChangeType {
    /// Authorize a new owner (op byte `0x01`).
    Authorize,
    /// Revoke an existing owner (op byte `0x02`).
    Revoke,
}

impl OwnerChangeType {
    /// Returns the on-wire op byte.
    pub const fn op_byte(&self) -> u8 {
        match self {
            Self::Authorize => Aa8130Constants::OWNER_CHANGE_AUTHORIZE,
            Self::Revoke => Aa8130Constants::OWNER_CHANGE_REVOKE,
        }
    }

    /// Parses a wire op byte.
    pub const fn from_op_byte(byte: u8) -> Option<Self> {
        match byte {
            Aa8130Constants::OWNER_CHANGE_AUTHORIZE => Some(Self::Authorize),
            Aa8130Constants::OWNER_CHANGE_REVOKE => Some(Self::Revoke),
            _ => None,
        }
    }
}

/// A single owner authorization or revocation inside a [`ConfigChange`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OwnerChange {
    /// Operation (authorize / revoke).
    pub change_type: OwnerChangeType,
    /// Verifier contract address.
    pub verifier: Address,
    /// Owner identifier.
    pub owner_id: B256,
    /// Scope bitmask (relevant for `Authorize`; ignored on `Revoke`).
    pub scope: Scope,
}

impl OwnerChange {
    fn rlp_fields_len(&self) -> usize {
        self.change_type.op_byte().length()
            + self.verifier.length()
            + self.owner_id.length()
            + self.scope.length()
    }
}

impl Encodable for OwnerChange {
    fn encode(&self, out: &mut dyn BufMut) {
        let fields_len = self.rlp_fields_len();
        let header = Header { list: true, payload_length: fields_len };
        header.encode(out);
        self.change_type.op_byte().encode(out);
        self.verifier.encode(out);
        self.owner_id.encode(out);
        self.scope.encode(out);
    }

    fn length(&self) -> usize {
        let fields_len = self.rlp_fields_len();
        length_of_length(fields_len) + fields_len
    }
}

impl Decodable for OwnerChange {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let started_len = buf.len();
        let op = u8::decode(buf)?;
        let change_type = OwnerChangeType::from_op_byte(op)
            .ok_or(alloy_rlp::Error::Custom("invalid OwnerChange op byte"))?;
        let verifier = Address::decode(buf)?;
        let owner_id = B256::decode(buf)?;
        let scope = Scope::decode(buf)?;
        let consumed = started_len - buf.len();
        if consumed != header.payload_length {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: consumed,
            });
        }
        Ok(Self { change_type, verifier, owner_id, scope })
    }
}

/// Body of an [`AccountChange::Create`] entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct CreateEntry {
    /// User-chosen salt used in the deterministic deploy address derivation.
    pub user_salt: B256,
    /// Account bytecode to install.
    pub code: Bytes,
    /// Initial owners authorized on the new account.
    pub initial_owners: Vec<InitialOwner>,
}

/// Body of an [`AccountChange::ConfigChange`] entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct ConfigChange {
    /// Chain ID this config change is bound to (replay protection).
    pub chain_id: u64,
    /// Per-account config-change sequence number.
    pub sequence: u64,
    /// Owner authorize/revoke operations applied in order.
    pub owner_changes: Vec<OwnerChange>,
    /// Authorization payload validated against an existing owner with `SCOPE_CONFIG`.
    pub auth: Bytes,
}

/// Body of an [`AccountChange::Delegation`] entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Delegation {
    /// Delegation target address. Zero means clear the existing delegation.
    pub target: Address,
}

/// A tagged-union entry inside `TxAa8130::account_changes`.
///
/// On the wire each entry is `type_byte || rlp([body_fields...])`:
/// - `0x00` -> [`AccountChange::Create`]
/// - `0x01` -> [`AccountChange::ConfigChange`]
/// - `0x02` -> [`AccountChange::Delegation`]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type", rename_all = "camelCase"))]
pub enum AccountChange {
    /// Create a new account.
    Create(CreateEntry),
    /// Change an existing account's owner set.
    ConfigChange(ConfigChange),
    /// Set or clear an [EIP-7702]-style delegation.
    ///
    /// [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702
    Delegation(Delegation),
}

impl AccountChange {
    /// Returns the on-wire type byte for this entry.
    pub const fn type_byte(&self) -> u8 {
        match self {
            Self::Create(_) => Aa8130Constants::ACCOUNT_CHANGE_TYPE_CREATE,
            Self::ConfigChange(_) => Aa8130Constants::ACCOUNT_CHANGE_TYPE_CONFIG,
            Self::Delegation(_) => Aa8130Constants::ACCOUNT_CHANGE_TYPE_DELEGATION,
        }
    }

    fn body_len(&self) -> usize {
        match self {
            Self::Create(b) => b.length(),
            Self::ConfigChange(b) => b.length(),
            Self::Delegation(b) => b.length(),
        }
    }
}

impl Encodable for AccountChange {
    fn encode(&self, out: &mut dyn BufMut) {
        out.put_u8(self.type_byte());
        match self {
            Self::Create(b) => b.encode(out),
            Self::ConfigChange(b) => b.encode(out),
            Self::Delegation(b) => b.encode(out),
        }
    }

    fn length(&self) -> usize {
        1 + self.body_len()
    }
}

impl Decodable for AccountChange {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let type_byte = buf[0];
        buf.advance(1);
        match type_byte {
            Aa8130Constants::ACCOUNT_CHANGE_TYPE_CREATE => {
                CreateEntry::decode(buf).map(Self::Create)
            }
            Aa8130Constants::ACCOUNT_CHANGE_TYPE_CONFIG => {
                ConfigChange::decode(buf).map(Self::ConfigChange)
            }
            Aa8130Constants::ACCOUNT_CHANGE_TYPE_DELEGATION => {
                Delegation::decode(buf).map(Self::Delegation)
            }
            _ => Err(alloy_rlp::Error::Custom("invalid AccountChange type byte")),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256, bytes};

    use super::*;

    #[test]
    fn scope_bit_helpers() {
        let s = Scope(
            Aa8130Constants::SCOPE_SIGNATURE
                | Aa8130Constants::SCOPE_SENDER
                | Aa8130Constants::SCOPE_PAYER
                | Aa8130Constants::SCOPE_CONFIG,
        );
        assert!(s.has_signature());
        assert!(s.has_sender());
        assert!(s.has_payer());
        assert!(s.has_config());
        assert!(!Scope::UNRESTRICTED.has_signature());
    }

    #[test]
    fn owner_change_type_roundtrip() {
        for ct in [OwnerChangeType::Authorize, OwnerChangeType::Revoke] {
            assert_eq!(OwnerChangeType::from_op_byte(ct.op_byte()), Some(ct));
        }
        assert_eq!(OwnerChangeType::from_op_byte(0x00), None);
        assert_eq!(OwnerChangeType::from_op_byte(0xff), None);
    }

    #[test]
    fn owner_change_rlp_roundtrip() {
        let oc = OwnerChange {
            change_type: OwnerChangeType::Authorize,
            verifier: address!("0x00000000000000000000000000000000000000aa"),
            owner_id: b256!("0x1111111111111111111111111111111111111111111111111111111111111111"),
            scope: Scope(Aa8130Constants::SCOPE_SIGNATURE | Aa8130Constants::SCOPE_SENDER),
        };
        let mut buf = Vec::new();
        oc.encode(&mut buf);
        assert_eq!(buf.len(), oc.length());
        let decoded = OwnerChange::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(oc, decoded);
    }

    #[test]
    fn account_change_create_roundtrip() {
        let ac = AccountChange::Create(CreateEntry {
            user_salt: b256!("0x2222222222222222222222222222222222222222222222222222222222222222"),
            code: bytes!("6080604052"),
            initial_owners: vec![InitialOwner {
                verifier: address!("0x00000000000000000000000000000000000000bb"),
                owner_id: b256!(
                    "0x3333333333333333333333333333333333333333333333333333333333333333"
                ),
                scope: Scope(Aa8130Constants::SCOPE_SIGNATURE),
            }],
        });
        let mut buf = Vec::new();
        ac.encode(&mut buf);
        assert_eq!(buf[0], Aa8130Constants::ACCOUNT_CHANGE_TYPE_CREATE);
        assert_eq!(buf.len(), ac.length());
        let decoded = AccountChange::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(ac, decoded);
    }

    #[test]
    fn account_change_config_roundtrip() {
        let ac = AccountChange::ConfigChange(ConfigChange {
            chain_id: 8453,
            sequence: 7,
            owner_changes: vec![OwnerChange {
                change_type: OwnerChangeType::Revoke,
                verifier: address!("0x00000000000000000000000000000000000000cc"),
                owner_id: b256!(
                    "0x4444444444444444444444444444444444444444444444444444444444444444"
                ),
                scope: Scope::UNRESTRICTED,
            }],
            auth: bytes!("aabbcc"),
        });
        let mut buf = Vec::new();
        ac.encode(&mut buf);
        assert_eq!(buf[0], Aa8130Constants::ACCOUNT_CHANGE_TYPE_CONFIG);
        let decoded = AccountChange::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(ac, decoded);
    }

    #[test]
    fn account_change_delegation_roundtrip() {
        let ac = AccountChange::Delegation(Delegation {
            target: address!("0x00000000000000000000000000000000000000dd"),
        });
        let mut buf = Vec::new();
        ac.encode(&mut buf);
        assert_eq!(buf[0], Aa8130Constants::ACCOUNT_CHANGE_TYPE_DELEGATION);
        let decoded = AccountChange::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(ac, decoded);
    }

    #[test]
    fn account_change_clear_delegation() {
        let ac = AccountChange::Delegation(Delegation { target: Address::ZERO });
        let mut buf = Vec::new();
        ac.encode(&mut buf);
        let decoded = AccountChange::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(ac, decoded);
    }

    #[test]
    fn account_change_invalid_type_byte() {
        let buf = [0xffu8, 0xc0];
        let mut slice = &buf[..];
        let res = AccountChange::decode(&mut slice);
        assert!(res.is_err());
    }

    #[test]
    fn scope_encodes_as_bare_uint8() {
        let mut buf = Vec::new();
        Scope(0x05).encode(&mut buf);
        assert_eq!(buf, vec![0x05], "Scope must serialize as a single RLP byte, not a list");

        let mut zero = Vec::new();
        Scope(0x00).encode(&mut zero);
        assert_eq!(zero, vec![0x80], "Zero byte RLP encodes as 0x80");

        let mut high = Vec::new();
        Scope(0x80).encode(&mut high);
        assert_eq!(high, vec![0x81, 0x80], "High-bit byte RLP encodes as 0x81 0x80");

        let mut slice = buf.as_slice();
        let decoded = Scope::decode(&mut slice).unwrap();
        assert_eq!(decoded, Scope(0x05));
        assert!(slice.is_empty());
    }
}
