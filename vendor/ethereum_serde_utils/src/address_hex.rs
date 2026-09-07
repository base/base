use alloy_primitives::Address;
use serde::{Deserializer, Serializer, de::Error};

use crate::hex::PrefixedHexVisitor;

pub fn serialize<S>(address: &Address, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut hex_string: String = "0x".to_string();
    hex_string.push_str(&hex::encode(&address));

    serializer.serialize_str(&hex_string)
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<Address, D::Error>
where
    D: Deserializer<'de>,
{
    let decoded = deserializer.deserialize_str(PrefixedHexVisitor)?;

    if decoded.len() != 20 {
        return Err(D::Error::custom(format!(
            "expected {} bytes for array, got {}",
            20,
            decoded.len()
        )));
    }

    let mut array = [0; 20];
    array.copy_from_slice(&decoded);
    Ok(array.into())
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use alloy_primitives::Address;
    use serde::{Deserialize, Serialize};
    use serde_json;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    struct Wrapper {
        #[serde(with = "super")]
        val: Address,
    }

    #[test]
    fn encoding() {
        assert_eq!(
            &serde_json::to_string(&Wrapper {
                val: Address::from_str("0000000000000000000000000000000000000000").unwrap()
            })
            .unwrap(),
            "\"0x0000000000000000000000000000000000000000\""
        );
        assert_eq!(
            &serde_json::to_string(&Wrapper {
                val: Address::from_str("0000000000000000000000000000000000000001").unwrap()
            })
            .unwrap(),
            "\"0x0000000000000000000000000000000000000001\""
        );
        assert_eq!(
            &serde_json::to_string(&Wrapper {
                val: Address::from_str("1000000000000000000000000000000000000000").unwrap()
            })
            .unwrap(),
            "\"0x1000000000000000000000000000000000000000\""
        );
        assert_eq!(
            &serde_json::to_string(&Wrapper {
                val: Address::from_str("1234567890000000000000000000000000000000").unwrap()
            })
            .unwrap(),
            "\"0x1234567890000000000000000000000000000000\""
        );
        assert_eq!(
            &serde_json::to_string(&Wrapper { val: Address::ZERO }).unwrap(),
            "\"0x0000000000000000000000000000000000000000\""
        );
    }

    #[test]
    fn decoding() {
        assert_eq!(
            serde_json::from_str::<Wrapper>("\"0x0000000000000000000000000000000000000000\"")
                .unwrap(),
            Wrapper { val: Address::ZERO },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>("\"0x0000000000000000000000000000000000000001\"")
                .unwrap(),
            Wrapper { val: Address::from_str("0000000000000000000000000000000000000001").unwrap() },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>("\"0x1000000000000000000000000000000000000000\"")
                .unwrap(),
            Wrapper { val: Address::from_str("1000000000000000000000000000000000000000").unwrap() },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>("\"0x1234567890000000000000000000000000000000\"")
                .unwrap(),
            Wrapper { val: Address::from_str("1234567890000000000000000000000000000000").unwrap() },
        );
        // Wrong length.
        serde_json::from_str::<Wrapper>("\"0x0\"").unwrap_err();
        serde_json::from_str::<Wrapper>("\"0x0400\"").unwrap_err();
        serde_json::from_str::<Wrapper>("\"0x12345678900000000000000000000000000000001\"")
            .unwrap_err();
        // Requires 0x.
        serde_json::from_str::<Wrapper>("\"1234567890000000000000000000000000000000\"")
            .unwrap_err();
        serde_json::from_str::<Wrapper>("\"ff34567890000000000000000000000000000000\"")
            .unwrap_err();
        // Contains invalid characters.
        serde_json::from_str::<Wrapper>("\"0x-100000000000000000000000000000000000000\"")
            .unwrap_err();
    }
}
