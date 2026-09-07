//! Formats `[u8; n]` as a 0x-prefixed hex string.
//!
//! E.g., `[0, 1, 2, 3]` serializes as `"0x00010203"`.

use serde::{Deserializer, Serializer, de::Error};

use crate::hex::PrefixedHexVisitor;

macro_rules! bytes_hex {
    ($num_bytes: tt) => {
        use super::*;

        const BYTES_LEN: usize = $num_bytes;

        pub fn serialize<S>(bytes: &[u8; BYTES_LEN], serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let mut hex_string: String = "0x".to_string();
            hex_string.push_str(&hex::encode(&bytes));

            serializer.serialize_str(&hex_string)
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; BYTES_LEN], D::Error>
        where
            D: Deserializer<'de>,
        {
            let decoded = deserializer.deserialize_str(PrefixedHexVisitor)?;

            if decoded.len() != BYTES_LEN {
                return Err(D::Error::custom(format!(
                    "expected {} bytes for array, got {}",
                    BYTES_LEN,
                    decoded.len()
                )));
            }

            let mut array = [0; BYTES_LEN];
            array.copy_from_slice(&decoded);
            Ok(array)
        }

        #[cfg(test)]
        mod test {
            use serde::{Deserialize, Serialize};

            use super::*;

            #[derive(Debug, PartialEq, Serialize, Deserialize)]
            #[serde(transparent)]
            struct Wrapper {
                #[serde(with = "super")]
                val: [u8; BYTES_LEN],
            }

            fn generate_string_value(v1: &str, v2: &str) -> String {
                let mut i = 0;
                let mut value = String::new();
                while i < BYTES_LEN * 2 {
                    if i % 2 == 0 {
                        value.push_str(v1);
                    } else {
                        value.push_str(v2);
                    }
                    i += 1;
                }
                value
            }

            #[test]
            fn encoding() {
                let zero = "0".repeat(BYTES_LEN * 2);
                assert_eq!(
                    &serde_json::to_string(&Wrapper { val: [0; BYTES_LEN] }).unwrap(),
                    &format!("\"0x{}\"", zero)
                );

                assert_eq!(
                    &serde_json::to_string(&Wrapper { val: [123; BYTES_LEN] }).unwrap(),
                    &format!("\"0x{}\"", generate_string_value("7", "b"))
                );

                let max = "f".repeat(BYTES_LEN * 2);
                assert_eq!(
                    &serde_json::to_string(&Wrapper { val: [u8::MAX; BYTES_LEN] }).unwrap(),
                    &format!("\"0x{}\"", max)
                );
            }

            #[test]
            fn decoding() {
                let zero = "0".repeat(BYTES_LEN * 2);
                assert_eq!(
                    serde_json::from_str::<Wrapper>(&format!("\"0x{}\"", zero)).unwrap(),
                    Wrapper { val: [0; BYTES_LEN] },
                );
                assert_eq!(
                    serde_json::from_str::<Wrapper>(&format!(
                        "\"0x{}\"",
                        generate_string_value("7", "b")
                    ))
                    .unwrap(),
                    Wrapper { val: [123; BYTES_LEN] },
                );

                let max = "f".repeat(BYTES_LEN * 2);
                assert_eq!(
                    serde_json::from_str::<Wrapper>(&format!("\"0x{}\"", max)).unwrap(),
                    Wrapper { val: [u8::MAX; BYTES_LEN] },
                );

                // Require 0x.
                serde_json::from_str::<Wrapper>(&format!("\"{}\"", "0".repeat(BYTES_LEN * 2)))
                    .unwrap_err();

                let exceed_max = "f".repeat((BYTES_LEN * 2) + 1);
                // Wrong length.
                serde_json::from_str::<Wrapper>(&format!("\"0x{}\"", exceed_max)).unwrap_err();
            }
        }
    };
}

pub mod bytes_4_hex {
    bytes_hex!(4);
}

pub mod bytes_8_hex {
    bytes_hex!(8);
}

pub mod bytes_16_hex {
    bytes_hex!(16);
}
