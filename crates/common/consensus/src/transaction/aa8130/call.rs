//! Per-call payload used inside the [EIP-8130] `calls` field.
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

use alloy_primitives::{Address, Bytes};
use alloy_rlp::{RlpDecodable, RlpEncodable};

/// A single call dispatched by the protocol during AA transaction execution.
///
/// Spec wire form: `rlp([to, data])` where `to` is a 20-byte address and `data`
/// is the calldata. The dispatched call carries no value (`msg.value == 0`);
/// ETH transfers must be performed by the wallet bytecode via the `CALL` opcode.
///
/// AA transactions group calls into phases (`Vec<Vec<Call>>`); see
/// [`super::tx::TxAa8130::calls`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Call {
    /// Recipient address of the call.
    pub to: Address,
    /// Calldata passed to the recipient.
    pub data: Bytes,
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
            data: bytes!("deadbeef"),
        };
        let mut buf = Vec::new();
        call.encode(&mut buf);
        let decoded = Call::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(call, decoded);
    }

    #[test]
    fn rlp_roundtrip_empty_data() {
        let call = Call { to: Address::ZERO, data: Bytes::new() };
        let mut buf = Vec::new();
        call.encode(&mut buf);
        let decoded = Call::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(call, decoded);
    }
}
