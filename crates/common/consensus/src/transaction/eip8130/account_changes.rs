//! [EIP-8130] `account_changes` entry types.
//!
//! An [`AccountChange`] is a tagged-union entry inside `TxEip8130::account_changes`.
//! On the wire, each entry is a single RLP list whose first element is the type
//! byte: `rlp([type_byte, entry_fields...])`.
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes};
use alloy_rlp::{
    BufMut, Decodable, Encodable, Header, RlpDecodable, RlpEncodable, length_of_length,
};

use crate::transaction::eip8130::constants::Eip8130Constants;

/// Bitmask describing the contexts in which an actor is valid.
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
    /// Unrestricted scope (actor valid in all contexts).
    pub const UNRESTRICTED: Self = Self(Eip8130Constants::SCOPE_UNRESTRICTED);

    /// Returns the raw bitmask.
    pub const fn bits(&self) -> u8 {
        self.0
    }

    /// Returns true if the scope grants the ungated `SCOPE_SENDER` context.
    pub const fn has_sender(&self) -> bool {
        self.0 & Eip8130Constants::SCOPE_SENDER != 0
    }

    /// Returns true if the scope enables policy gating.
    pub const fn has_policy(&self) -> bool {
        self.0 & Eip8130Constants::SCOPE_POLICY != 0
    }

    /// Returns true if the scope grants the nonce context.
    pub const fn has_nonce(&self) -> bool {
        self.0 & Eip8130Constants::SCOPE_NONCE != 0
    }

    /// Returns true if the scope grants self-pay (`payer == sender`).
    pub const fn has_self_payer(&self) -> bool {
        self.0 & Eip8130Constants::SCOPE_SELF_PAYER != 0
    }

    /// Returns true if the scope grants sponsorship (`payer != sender`).
    pub const fn has_sponsor_payer(&self) -> bool {
        self.0 & Eip8130Constants::SCOPE_SPONSOR_PAYER != 0
    }
}

/// Initial actor installed on a newly-created account.
///
/// Per [EIP-8130], a create entry's initial actors are
/// `[actorId, authenticator, scope, policyData]` tuples. `scope` is stored
/// verbatim (`0x00` = unrestricted admin; unknown bits allowed), and `policyData`
/// is empty unless `scope` sets `POLICY`, in which case it is exactly
/// `manager (20) || commitment (32)`. Two things are deliberately **not**
/// expressible here and are added afterwards via a [`ConfigChange`]: `expiry`
/// (initial actors are always non-expiring, committed as `0`) and a
/// self-referential `manager = account` (the account address is not known at
/// commitment time). The field order matches the wire encoding and the
/// address-derivation commitment (`actorId || authenticator || scope ||
/// policyData`).
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug, Clone, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct InitialActor {
    /// Actor identifier (the authenticator-derived `actorId`).
    pub actor_id: B256,
    /// Address of the authenticator contract (e.g. an ERC-1271 authenticator).
    pub authenticator: Address,
    /// Scope byte (`0x00` = unrestricted admin), stored verbatim.
    pub scope: u8,
    /// Policy data: empty unless `scope` sets `POLICY`, otherwise exactly
    /// `manager (20) || commitment (32)`. Committed to the derived address.
    pub policy_data: Bytes,
}

impl InitialActor {
    /// Constructs an unrestricted admin initial actor (`scope == 0x00`, no
    /// policy), the common owner case.
    pub const fn owner(actor_id: B256, authenticator: Address) -> Self {
        Self { actor_id, authenticator, scope: 0, policy_data: Bytes::new() }
    }
}

/// Operation performed by an [`ActorChange`] inside a [`ConfigChange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ActorChangeType {
    /// Authorize a new actor (op byte `0x01`).
    Authorize,
    /// Revoke an existing actor (op byte `0x02`).
    Revoke,
}

impl ActorChangeType {
    /// Returns the on-wire op byte.
    pub const fn op_byte(&self) -> u8 {
        match self {
            Self::Authorize => Eip8130Constants::ACTOR_CHANGE_AUTHORIZE,
            Self::Revoke => Eip8130Constants::ACTOR_CHANGE_REVOKE,
        }
    }

    /// Parses a wire op byte.
    pub const fn from_op_byte(byte: u8) -> Option<Self> {
        match byte {
            Eip8130Constants::ACTOR_CHANGE_AUTHORIZE => Some(Self::Authorize),
            Eip8130Constants::ACTOR_CHANGE_REVOKE => Some(Self::Revoke),
            _ => None,
        }
    }
}

/// A single actor authorization or revocation inside a [`ConfigChange`].
///
/// Per [EIP-8130], `data` is the operation-specific, contract-ABI-encoded blob:
/// `abi.encode(ActorConfig, bytes policyData)` for an `Authorize` (carrying the
/// new actor's authenticator, scope, expiry, and policy data), and
/// empty for a `Revoke`. It is opaque at this layer — decoded only where the
/// change is applied (native authorization or `applySignedActorChanges`) — and
/// is the value hashed (`keccak256(data)`) in the config-change signature
/// payload, so the same blob is interpreted identically on every path.
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct ActorChange {
    /// Operation (authorize / revoke).
    pub change_type: ActorChangeType,
    /// Actor identifier.
    pub actor_id: B256,
    /// Operation-specific ABI-encoded data (opaque at this layer).
    pub data: Bytes,
}

impl ActorChange {
    fn rlp_fields_len(&self) -> usize {
        self.change_type.op_byte().length() + self.actor_id.length() + self.data.length()
    }
}

impl Encodable for ActorChange {
    fn encode(&self, out: &mut dyn BufMut) {
        let fields_len = self.rlp_fields_len();
        let header = Header { list: true, payload_length: fields_len };
        header.encode(out);
        self.change_type.op_byte().encode(out);
        self.actor_id.encode(out);
        self.data.encode(out);
    }

    fn length(&self) -> usize {
        let fields_len = self.rlp_fields_len();
        length_of_length(fields_len) + fields_len
    }
}

impl Decodable for ActorChange {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let started_len = buf.len();
        let op = u8::decode(buf)?;
        let change_type = ActorChangeType::from_op_byte(op)
            .ok_or(alloy_rlp::Error::Custom("invalid ActorChange op byte"))?;
        let actor_id = B256::decode(buf)?;
        let data = Bytes::decode(buf)?;
        let consumed = started_len - buf.len();
        if consumed != header.payload_length {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: consumed,
            });
        }
        Ok(Self { change_type, actor_id, data })
    }
}

/// Body of an [`AccountChange::Create`] entry.
///
/// This struct has no standalone RLP codec: on the wire a create entry is a
/// single flat list `rlp([type_byte, user_salt, code, initial_actors])`, encoded
/// by [`AccountChange`]. See that type for the wire format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct CreateEntry {
    /// User-chosen salt used in the deterministic deploy address derivation.
    pub user_salt: B256,
    /// Account bytecode to install.
    pub code: Bytes,
    /// Initial actors authorized on the new account.
    pub initial_actors: Vec<InitialActor>,
}

/// Body of an [`AccountChange::ConfigChange`] entry.
///
/// This struct has no standalone RLP codec: on the wire a config-change entry is
/// a single flat list `rlp([type_byte, chain_id, sequence, actor_changes, auth])`,
/// encoded by [`AccountChange`]. See that type for the wire format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct ConfigChange {
    /// Chain ID this config change is bound to (replay protection).
    pub chain_id: u64,
    /// Per-account config-change sequence number.
    pub sequence: u64,
    /// Actor authorize/revoke operations applied in order.
    pub actor_changes: Vec<ActorChange>,
    /// Authorization payload validated against an admin actor (`scope == 0`).
    pub auth: Bytes,
}

/// Body of an [`AccountChange::Delegation`] entry.
///
/// This struct has no standalone RLP codec: on the wire a delegation entry is a
/// single flat list `rlp([type_byte, target])`, encoded by [`AccountChange`]. See
/// that type for the wire format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Delegation {
    /// Delegation target address. Zero means clear the existing delegation.
    pub target: Address,
}

/// A tagged-union entry inside `TxEip8130::account_changes`.
///
/// On the wire each entry is a single RLP list whose first element is the type
/// byte, followed by the body fields inline (per [EIP-8130]):
/// - `rlp([0x00, user_salt, code, initial_actors])` -> [`AccountChange::Create`]
/// - `rlp([0x01, chain_id, sequence, actor_changes, auth])` -> [`AccountChange::ConfigChange`]
/// - `rlp([0x02, target])` -> [`AccountChange::Delegation`]
///
/// The type byte is a genuine list element (not an EIP-2718-style `type_byte ||
/// rlp(...)` prefix), so each entry is one self-contained RLP item and the
/// surrounding `account_changes` list frames as one item per entry. This mirrors
/// the sibling [`ActorChange`] codec, which likewise carries its discriminant as
/// the first list element.
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type", rename_all = "camelCase"))]
pub enum AccountChange {
    /// Create a new account.
    Create(CreateEntry),
    /// Change an existing account's actor set.
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
            Self::Create(_) => Eip8130Constants::ACCOUNT_CHANGE_TYPE_CREATE,
            Self::ConfigChange(_) => Eip8130Constants::ACCOUNT_CHANGE_TYPE_CONFIG,
            Self::Delegation(_) => Eip8130Constants::ACCOUNT_CHANGE_TYPE_DELEGATION,
        }
    }

    /// Length of the RLP list payload: the type byte followed by the body
    /// fields, all inline in one list (the type byte is a list element, so it is
    /// RLP-encoded, e.g. `0x00` -> `0x80`).
    fn rlp_payload_length(&self) -> usize {
        let fields_len = match self {
            Self::Create(b) => b.user_salt.length() + b.code.length() + b.initial_actors.length(),
            Self::ConfigChange(b) => {
                b.chain_id.length()
                    + b.sequence.length()
                    + b.actor_changes.length()
                    + b.auth.length()
            }
            Self::Delegation(b) => b.target.length(),
        };
        self.type_byte().length() + fields_len
    }
}

impl Encodable for AccountChange {
    fn encode(&self, out: &mut dyn BufMut) {
        let payload_length = self.rlp_payload_length();
        Header { list: true, payload_length }.encode(out);
        self.type_byte().encode(out);
        match self {
            Self::Create(b) => {
                b.user_salt.encode(out);
                b.code.encode(out);
                b.initial_actors.encode(out);
            }
            Self::ConfigChange(b) => {
                b.chain_id.encode(out);
                b.sequence.encode(out);
                b.actor_changes.encode(out);
                b.auth.encode(out);
            }
            Self::Delegation(b) => {
                b.target.encode(out);
            }
        }
    }

    fn length(&self) -> usize {
        let payload_length = self.rlp_payload_length();
        length_of_length(payload_length) + payload_length
    }
}

impl Decodable for AccountChange {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let started_len = buf.len();
        let type_byte = u8::decode(buf)?;
        let this = match type_byte {
            Eip8130Constants::ACCOUNT_CHANGE_TYPE_CREATE => Self::Create(CreateEntry {
                user_salt: B256::decode(buf)?,
                code: Bytes::decode(buf)?,
                initial_actors: Vec::<InitialActor>::decode(buf)?,
            }),
            Eip8130Constants::ACCOUNT_CHANGE_TYPE_CONFIG => Self::ConfigChange(ConfigChange {
                chain_id: u64::decode(buf)?,
                sequence: u64::decode(buf)?,
                actor_changes: Vec::<ActorChange>::decode(buf)?,
                auth: Bytes::decode(buf)?,
            }),
            Eip8130Constants::ACCOUNT_CHANGE_TYPE_DELEGATION => {
                Self::Delegation(Delegation { target: Address::decode(buf)? })
            }
            _ => return Err(alloy_rlp::Error::Custom("invalid AccountChange type byte")),
        };
        let consumed = started_len - buf.len();
        if consumed != header.payload_length {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: consumed,
            });
        }
        Ok(this)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256, bytes};

    use super::*;

    #[test]
    fn scope_bit_helpers() {
        let s = Scope(
            Eip8130Constants::SCOPE_SENDER
                | Eip8130Constants::SCOPE_POLICY
                | Eip8130Constants::SCOPE_NONCE
                | Eip8130Constants::SCOPE_SELF_PAYER
                | Eip8130Constants::SCOPE_SPONSOR_PAYER,
        );
        assert!(s.has_sender());
        assert!(s.has_policy());
        assert!(s.has_nonce());
        assert!(s.has_self_payer());
        assert!(s.has_sponsor_payer());
        assert!(!Scope::UNRESTRICTED.has_sender());
    }

    #[test]
    fn actor_change_type_roundtrip() {
        for ct in [ActorChangeType::Authorize, ActorChangeType::Revoke] {
            assert_eq!(ActorChangeType::from_op_byte(ct.op_byte()), Some(ct));
        }
        assert_eq!(ActorChangeType::from_op_byte(0x00), None);
        assert_eq!(ActorChangeType::from_op_byte(0xff), None);
    }

    #[test]
    fn actor_change_rlp_roundtrip() {
        let oc = ActorChange {
            change_type: ActorChangeType::Authorize,
            actor_id: b256!("0x1111111111111111111111111111111111111111111111111111111111111111"),
            data: bytes!("00000000000000000000000000000000000000000000000000000000000000aa"),
        };
        let mut buf = Vec::new();
        oc.encode(&mut buf);
        assert_eq!(buf.len(), oc.length());
        let decoded = ActorChange::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(oc, decoded);
    }

    #[test]
    fn account_change_create_roundtrip() {
        let ac = AccountChange::Create(CreateEntry {
            user_salt: b256!("0x2222222222222222222222222222222222222222222222222222222222222222"),
            code: bytes!("6080604052"),
            initial_actors: vec![InitialActor {
                actor_id: b256!(
                    "0x3333333333333333333333333333333333333333333333333333333333333333"
                ),
                authenticator: address!("0x00000000000000000000000000000000000000bb"),
                scope: Eip8130Constants::SCOPE_POLICY,
                policy_data: bytes!(
                    "00000000000000000000000000000000000000cc4444444444444444444444444444444444444444444444444444444444444444"
                ),
            }],
        });
        let mut buf = Vec::new();
        ac.encode(&mut buf);
        assert_eq!(
            first_list_element_type_byte(&buf),
            Eip8130Constants::ACCOUNT_CHANGE_TYPE_CREATE
        );
        assert_eq!(buf.len(), ac.length());
        let decoded = AccountChange::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(ac, decoded);
    }

    #[test]
    fn account_change_config_roundtrip() {
        let ac = AccountChange::ConfigChange(ConfigChange {
            chain_id: 8453,
            sequence: 7,
            actor_changes: vec![ActorChange {
                change_type: ActorChangeType::Revoke,
                actor_id: b256!(
                    "0x4444444444444444444444444444444444444444444444444444444444444444"
                ),
                data: Bytes::new(),
            }],
            auth: bytes!("aabbcc"),
        });
        let mut buf = Vec::new();
        ac.encode(&mut buf);
        assert_eq!(
            first_list_element_type_byte(&buf),
            Eip8130Constants::ACCOUNT_CHANGE_TYPE_CONFIG
        );
        assert_eq!(buf.len(), ac.length());
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
        assert_eq!(
            first_list_element_type_byte(&buf),
            Eip8130Constants::ACCOUNT_CHANGE_TYPE_DELEGATION
        );
        assert_eq!(buf.len(), ac.length());
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
        // A well-formed RLP list `[0x7f]` (header 0xc1, element 0x7f) carrying an
        // unrecognized type byte must be rejected by the type-byte match arm.
        let buf = [0xc1u8, 0x7f];
        let mut slice = &buf[..];
        let res = AccountChange::decode(&mut slice);
        assert!(res.is_err());
    }

    #[test]
    fn account_change_entry_is_single_rlp_item() {
        // Each entry must be exactly one self-contained RLP list item: the type
        // discriminant lives *inside* the list (spec `rlp([type, ...])`), never as
        // a bare prefix byte. A bare prefix would split the entry into two RLP
        // tokens and desync the surrounding `account_changes` list framing.
        let ac = AccountChange::Delegation(Delegation {
            target: address!("0x00000000000000000000000000000000000000dd"),
        });
        let mut buf = Vec::new();
        ac.encode(&mut buf);

        let mut slice = buf.as_slice();
        let header = Header::decode(&mut slice).unwrap();
        assert!(header.list, "an account-change entry must be a single RLP list");
        // The list header must account for the entire entry, so decoding it
        // leaves exactly its payload and nothing trailing.
        assert_eq!(slice.len(), header.payload_length);
    }

    #[test]
    fn account_changes_vec_frames_one_item_per_entry() {
        // Regression guard for the framing: the outer `account_changes` list must
        // contain exactly one RLP item per entry. Under a bare `type_byte ||
        // rlp(body)` encoding a generic RLP walk would see two items per entry.
        let entries = vec![
            AccountChange::Delegation(Delegation { target: Address::ZERO }),
            AccountChange::ConfigChange(ConfigChange {
                chain_id: 1,
                sequence: 0,
                actor_changes: Vec::new(),
                auth: Bytes::new(),
            }),
            AccountChange::Create(CreateEntry {
                user_salt: b256!(
                    "0x2222222222222222222222222222222222222222222222222222222222222222"
                ),
                code: bytes!("6080604052"),
                initial_actors: vec![InitialActor::owner(
                    b256!("0x3333333333333333333333333333333333333333333333333333333333333333"),
                    address!("0x00000000000000000000000000000000000000bb"),
                )],
            }),
        ];

        let mut buf = Vec::new();
        entries.encode(&mut buf);

        let mut slice = buf.as_slice();
        let outer = Header::decode(&mut slice).unwrap();
        assert!(outer.list);
        let mut payload = &slice[..outer.payload_length];
        let mut count = 0usize;
        while !payload.is_empty() {
            let item = Header::decode(&mut payload).unwrap();
            payload = &payload[item.payload_length..];
            count += 1;
        }
        assert_eq!(count, entries.len(), "each entry must frame as exactly one RLP item");

        let decoded = Vec::<AccountChange>::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(decoded, entries);
    }

    /// Decodes the outer RLP list header of an encoded [`AccountChange`] and
    /// returns its first element (the type byte).
    fn first_list_element_type_byte(encoded: &[u8]) -> u8 {
        let mut slice = encoded;
        let header = Header::decode(&mut slice).unwrap();
        assert!(header.list, "an account-change entry must be a single RLP list");
        u8::decode(&mut slice).unwrap()
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
