use core::fmt;

use alloy_primitives::{Address, Bytes};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error, Visitor},
};

use super::Bytecode;

// TODO: eventually remove `BytecodeSerdeOld`.

#[derive(Deserialize)]
#[serde(untagged)]
enum BytecodeSerde {
    New(Bytes),
    Old(BytecodeSerdeOld),
}

#[derive(Deserialize)]
enum BytecodeSerdeOld {
    LegacyAnalyzed { bytecode: Bytes, original_len: usize },
    Eip7702 { delegated_address: Address },
}

impl Serialize for Bytecode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            self.original_bytes().serialize(serializer)
        } else {
            serializer.serialize_bytes(self.original_byte_slice())
        }
    }
}

impl<'de> Deserialize<'de> for Bytecode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            match BytecodeSerde::deserialize(deserializer)? {
                BytecodeSerde::New(bytes) => {
                    Self::new_raw_checked(bytes).map_err(serde::de::Error::custom)
                }
                BytecodeSerde::Old(bytecode_serde_old) => match bytecode_serde_old {
                    BytecodeSerdeOld::LegacyAnalyzed { bytecode, original_len } => {
                        if original_len > bytecode.len() {
                            return Err(serde::de::Error::custom(
                                "original_len is greater than bytecode length",
                            ));
                        }
                        Ok(Self::new_legacy(bytecode.slice(..original_len)))
                    }
                    BytecodeSerdeOld::Eip7702 { delegated_address } => {
                        Ok(Self::new_eip7702(delegated_address))
                    }
                },
            }
        } else {
            deserializer.deserialize_bytes(BytecodeVisitor)
        }
    }
}

struct BytecodeVisitor;

impl Visitor<'_> for BytecodeVisitor {
    type Value = Bytecode;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EVM bytecode")
    }

    fn visit_bytes<E: Error>(self, value: &[u8]) -> Result<Self::Value, E> {
        Bytecode::new_raw_checked(Bytes::copy_from_slice(value)).map_err(Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex;

    use super::*;
    use crate::bytecode::BytecodeKind;

    #[test]
    fn serde_roundtrip() {
        for bytes in [&hex!()[..], &hex!("0x1234")[..]] {
            let bytecode = Bytecode::new_raw_checked(bytes.to_vec().into()).unwrap();
            let json = serde_json::to_string(&bytecode).unwrap();
            let deserialized: Bytecode = serde_json::from_str(&json).unwrap();

            assert_eq!(deserialized.kind(), BytecodeKind::Legacy);
            assert_eq!(deserialized.eip7702_address(), None);
            assert_eq!(deserialized.original_byte_slice(), bytes);
        }
    }

    #[test]
    fn serde_roundtrip_7702() {
        let delegated_address = Address::with_last_byte(0x42);
        let bytecode = Bytecode::new_eip7702(delegated_address);

        let json = serde_json::to_string(&bytecode).unwrap();
        let deserialized: Bytecode = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.kind(), BytecodeKind::Eip7702);
        assert_eq!(deserialized.eip7702_address(), Some(delegated_address));
    }

    #[test]
    fn serde_binary_roundtrip() {
        for bytes in [&hex!()[..], &hex!("0x1234")[..]] {
            let bytecode = Bytecode::new_raw_checked(bytes.to_vec().into()).unwrap();
            let encoded = postcard::to_allocvec(&bytecode).unwrap();
            let deserialized: Bytecode = postcard::from_bytes(&encoded).unwrap();

            assert_eq!(deserialized.kind(), BytecodeKind::Legacy);
            assert_eq!(deserialized.eip7702_address(), None);
            assert_eq!(deserialized.original_byte_slice(), bytes);
        }
    }

    #[test]
    fn serde_revm_json_cases() {
        let cases = [
            (
                r#"{"LegacyAnalyzed":{"bytecode":"0x1234","original_len":2,"jump_table":{"order":"bitvec::order::Lsb0","head":{"width":8,"index":0},"bits":0,"data":[]}}}"#,
                &hex!("1234")[..],
                BytecodeKind::Legacy,
                None,
            ),
            (
                r#"{"Eip7702":{"delegated_address":"0x0000000000000000000000000000000000000042"}}"#,
                &hex!("ef01000000000000000000000000000000000000000042")[..],
                BytecodeKind::Eip7702,
                Some(Address::with_last_byte(0x42)),
            ),
        ];

        for (json, bytes, kind, delegated_address) in cases {
            let bytecode: Bytecode = serde_json::from_str(json).unwrap();

            assert_eq!(bytecode.kind(), kind);
            assert_eq!(bytecode.eip7702_address(), delegated_address);
            assert_eq!(bytecode.original_byte_slice(), bytes);

            let bytecode_json = serde_json::to_string(&bytecode).unwrap();
            let bytecode_roundtrip: Bytecode = serde_json::from_str(&bytecode_json).unwrap();
            assert_eq!(bytecode_roundtrip.kind(), kind);
            assert_eq!(bytecode_roundtrip.eip7702_address(), delegated_address);
            assert_eq!(bytecode_roundtrip.original_byte_slice(), bytes);
        }
    }
}
