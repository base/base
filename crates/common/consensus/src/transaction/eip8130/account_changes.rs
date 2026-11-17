//! [EIP-8130] `account_changes` entry types.
//!
//! An [`AccountChange`] is a tagged-union entry inside `TxEip8130::account_changes`.
//! On the wire, each entry is a single RLP list whose first element is the type
//! byte: `rlp([type_byte, entry_fields...])`.
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

use alloy_primitives::Address;
use alloy_rlp::{BufMut, Decodable, Encodable, Header, length_of_length};

use crate::transaction::eip8130::constants::Eip8130Constants;

/// Body of an [`AccountChange::Delegation`] entry.
///
/// A transaction carrying a delegation must use the protocol nonce
/// (`nonce_key == 0`), so the delegated account always ends with a non-zero
/// account nonce, as with EIP-7702.
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
/// `0x00` and `0x02` onwards are reserved by the AA transaction type and do not
/// decode, so a transaction carrying one is invalid.
///
/// In JSON the entry is an object whose `type` field is the type byte as a hex
/// quantity (`"0x1"`; `"0x01"` is also accepted), alongside the body fields.
///
/// The type byte is a genuine list element (not an EIP-2718-style `type_byte ||
/// rlp(...)` prefix), so each entry is one self-contained RLP item and the
/// surrounding `account_changes` list frames as one item per entry.
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type"))]
pub enum AccountChange {
    /// Set or clear an [EIP-7702]-style delegation.
    ///
    /// [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702
    #[cfg_attr(feature = "serde", serde(rename = "0x1", alias = "0x01"))]
    Delegation(Delegation),
}

impl AccountChange {
    /// Returns the on-wire type byte for this entry.
    pub const fn type_byte(&self) -> u8 {
        match self {
            Self::Delegation(_) => Eip8130Constants::ACCOUNT_CHANGE_TYPE_DELEGATION,
        }
    }

    /// Length of the RLP list payload: the type byte followed by the body
    /// fields, all inline in one list.
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
            Self::Delegation(b) => b.target.encode(out),
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
    fn reserved_type_bytes_do_not_decode() {
        // `[0x00]` is the RLP list `0xc1 0x80`.
        let buf = [0xc1u8, 0x80];
        assert!(AccountChange::decode(&mut &buf[..]).is_err());

        // `0x02` and `0x03` are reserved too: well-formed RLP entries with
        // those type bytes are rejected.
        let mut type_02 = Vec::new();
        Header { list: true, payload_length: 1 + 33 + 6 + 1 }.encode(&mut type_02);
        0x02u8.encode(&mut type_02);
        b256!("0x2222222222222222222222222222222222222222222222222222222222222222")
            .encode(&mut type_02);
        bytes!("6080604052").encode(&mut type_02);
        Header { list: true, payload_length: 0 }.encode(&mut type_02);
        assert!(AccountChange::decode(&mut type_02.as_slice()).is_err());

        let mut type_03 = Vec::new();
        Header { list: true, payload_length: 5 }.encode(&mut type_03);
        for field in [0x03u8, 0x00, 0x07] {
            field.encode(&mut type_03);
        }
        Header { list: true, payload_length: 0 }.encode(&mut type_03);
        bytes!("").encode(&mut type_03);
        assert!(AccountChange::decode(&mut type_03.as_slice()).is_err());
    }

    /// JSON names each entry by its type byte, as a hex quantity.
    #[cfg(feature = "serde")]
    #[test]
    fn account_change_json_type_is_the_type_byte() {
        let target = Address::repeat_byte(0x11);
        let delegation = AccountChange::Delegation(Delegation { target });
        let json = serde_json::to_value(&delegation).unwrap();
        assert_eq!(json, serde_json::json!({ "type": "0x1", "target": target }));

        for tag in ["0x1", "0x01"] {
            let parsed: AccountChange =
                serde_json::from_value(serde_json::json!({ "type": tag, "target": target }))
                    .unwrap();
            assert_eq!(parsed, delegation);
        }
        for tag in ["delegation", "0x2", "0x3"] {
            assert!(
                serde_json::from_value::<AccountChange>(
                    serde_json::json!({ "type": tag, "target": target })
                )
                .is_err()
            );
        }
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

    /// Decodes the outer RLP list header of an encoded [`AccountChange`] and
    /// returns its first element (the type byte).
    fn first_list_element_type_byte(encoded: &[u8]) -> u8 {
        let mut slice = encoded;
        let header = Header::decode(&mut slice).unwrap();
        assert!(header.list, "an account-change entry must be a single RLP list");
        u8::decode(&mut slice).unwrap()
    }
}
