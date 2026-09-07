use alloy_primitives::B256;
use serde::{Deserializer, Serializer, de::Error};

use crate::hex::PrefixedHexVisitor;

pub fn serialize<S>(hash: &B256, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut hex_string: String = "0x".to_string();
    hex_string.push_str(&hex::encode(&hash));

    serializer.serialize_str(&hex_string)
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<B256, D::Error>
where
    D: Deserializer<'de>,
{
    let decoded = deserializer.deserialize_str(PrefixedHexVisitor)?;

    if decoded.len() != 32 {
        return Err(D::Error::custom(format!(
            "expected {} bytes for array, got {}",
            32,
            decoded.len()
        )));
    }

    let mut array = [0; 32];
    array.copy_from_slice(&decoded);
    Ok(array.into())
}

#[cfg(test)]
mod test {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    struct Wrapper {
        #[serde(with = "super")]
        val: B256,
    }

    #[test]
    fn encoding() {
        assert_eq!(
            &serde_json::to_string(&Wrapper { val: B256::ZERO }).unwrap(),
            "\"0x0000000000000000000000000000000000000000000000000000000000000000\""
        );
        assert_eq!(
            &serde_json::to_string(&Wrapper { val: B256::with_last_byte(0x03) }).unwrap(),
            "\"0x0000000000000000000000000000000000000000000000000000000000000003\""
        );
        assert_eq!(
            &serde_json::to_string(&Wrapper { val: B256::repeat_byte(0x03) }).unwrap(),
            "\"0x0303030303030303030303030303030303030303030303030303030303030303\""
        );
    }

    #[test]
    fn decoding() {
        assert_eq!(
            serde_json::from_str::<Wrapper>(
                "\"0x0000000000000000000000000000000000000000000000000000000000000000\""
            )
            .unwrap(),
            Wrapper { val: B256::ZERO },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>(
                "\"0x0000000000000000000000000000000000000000000000000000000000000003\""
            )
            .unwrap(),
            Wrapper { val: B256::with_last_byte(0x03) },
        );

        // Require 0x.
        serde_json::from_str::<Wrapper>(
            "\"0000000000000000000000000000000000000000000000000000000000000000\"",
        )
        .unwrap_err();
        // Wrong length.
        serde_json::from_str::<Wrapper>(
            "\"0x00000000000000000000000000000000000000000000000000000000000000\"",
        )
        .unwrap_err();
    }
}
