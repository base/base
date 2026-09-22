//! Per-call payload used inside the [EIP-8130] `calls` field.
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

use alloy_primitives::{Address, Bytes, U256};
use alloy_rlp::{RlpDecodable, RlpEncodable};

/// A single call dispatched by the protocol during AA transaction execution.
///
/// Spec wire form: `rlp([to, value, data])` where `to` is a 20-byte address,
/// `value` is the wei transferred to `to` (a minimal big-endian RLP integer),
/// and `data` is the calldata. The dispatched call moves `value` from the
/// transaction `sender` to `call.to` with `tx.origin == sender`; a call whose
/// `value` exceeds the sender's spendable balance reverts its phase like any
/// other `CALL`.
///
/// AA transactions group calls into phases (`Vec<Vec<Call>>`); see
/// [`super::tx::TxEip8130::calls`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Call {
    /// Recipient address of the call.
    pub to: Address,
    /// Wei transferred from the transaction sender to `to` when the call is
    /// dispatched. Encoded as a minimal big-endian RLP integer.
    ///
    /// The RLP wire always carries this word (it is part of the signed
    /// preimage), but it is optional in JSON: an RPC `calls` entry that omits
    /// `value` defaults to zero, so pre-value clients keep working.
    #[cfg_attr(feature = "serde", serde(default))]
    pub value: U256,
    /// Calldata passed to the recipient.
    pub data: Bytes,
}

impl Call {
    /// Heap bytes owned beyond the [`Call`] slot itself (`data` payload). The
    /// `value` word is stored inline, so it adds no heap cost.
    pub fn heap_size(&self) -> usize {
        self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, bytes};
    use alloy_rlp::{Decodable, Encodable};

    use super::*;

    #[test]
    fn rlp_roundtrip() {
        let call = Call {
            to: address!("0x00000000000000000000000000000000000000aa"),
            value: U256::from(1_000_000_000_000_000_000u64),
            data: bytes!("deadbeef"),
        };
        let mut buf = Vec::new();
        call.encode(&mut buf);
        let decoded = Call::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(call, decoded);
    }

    #[test]
    fn rlp_roundtrip_empty_data() {
        let call = Call { to: Address::ZERO, value: U256::ZERO, data: Bytes::new() };
        let mut buf = Vec::new();
        call.encode(&mut buf);
        let decoded = Call::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(call, decoded);
    }
}
