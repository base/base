//! [EIP-8130] `account_changes` entry types.
//!
//! An [`AccountChange`] is a tagged-union entry inside `TxEip8130::account_changes`.
//! On the wire, each entry is a single RLP list whose first element is the type
//! byte: `rlp([type_byte, entry_fields...])`.
//!
//! The launch wire supports exactly one account change: an [EIP-7702]-style
//! [`Delegation`]. The Keystore's account-creation and signed-config-change
//! entries have been removed, so [`AccountChange`] is a single-variant tagged
//! union that keeps the on-wire `account_changes` framing (`type_byte` inside a
//! per-entry RLP list) for delegation.
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
//! [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702

use alloy_primitives::Address;
use alloy_rlp::{BufMut, Decodable, Encodable, Header, length_of_length};

use crate::transaction::eip8130::constants::Eip8130Constants;

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
/// - `rlp([0x01, target])` -> [`AccountChange::Delegation`]
///
/// The type byte is a genuine list element (not an EIP-2718-style `type_byte ||
/// rlp(...)` prefix), so each entry is one self-contained RLP item and the
/// surrounding `account_changes` list frames as one item per entry.
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type", rename_all = "camelCase"))]
pub enum AccountChange {
    /// Set or clear an [EIP-7702]-style delegation.
    ///
    /// [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702
    Delegation(Delegation),
}

impl AccountChange {
    /// Returns the on-wire type byte for this entry.
    pub const fn type_byte(&self) -> u8 {
        match self {
            Self::Delegation(_) => Eip8130Constants::ACCOUNT_CHANGE_TYPE_DELEGATION,
        }
    }

    /// Heap bytes owned beyond the enum slot itself.
    ///
    /// The surrounding [`alloc::vec::Vec`] capacity × [`core::mem::size_of`]`<Self>`
    /// covers the inline layout; a delegation owns no additional heap bytes.
    pub const fn heap_size(&self) -> usize {
        match self {
            Self::Delegation(_) => 0,
        }
    }

    /// Length of the RLP list payload: the type byte followed by the body
    /// fields, all inline in one list (the type byte is a list element, so it is
    /// RLP-encoded, e.g. `0x01` -> `0x01`).
    fn rlp_payload_length(&self) -> usize {
        let fields_len = match self {
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
        // Delegation (`0x01`) is the only account change on the launch wire.
        let this = match type_byte {
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
    use alloc::{vec, vec::Vec};

    use alloy_primitives::address;

    use super::*;

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
        // a bare prefix byte.
        let ac = AccountChange::Delegation(Delegation {
            target: address!("0x00000000000000000000000000000000000000dd"),
        });
        let mut buf = Vec::new();
        ac.encode(&mut buf);

        let mut slice = buf.as_slice();
        let header = Header::decode(&mut slice).unwrap();
        assert!(header.list, "an account-change entry must be a single RLP list");
        assert_eq!(slice.len(), header.payload_length);
    }

    #[test]
    fn account_changes_vec_frames_one_item_per_entry() {
        // The outer `account_changes` list must contain exactly one RLP item per
        // entry.
        let entries = vec![
            AccountChange::Delegation(Delegation { target: Address::ZERO }),
            AccountChange::Delegation(Delegation {
                target: address!("0x00000000000000000000000000000000000000bb"),
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
}
