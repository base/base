//! Formats `u8` as a 0x-prefixed hex string.
//!
//! E.g., `0` serializes as `"0x00"`.

use serde::{Deserializer, Serializer, de::Error};

use crate::hex::PrefixedHexVisitor;

pub fn serialize<S>(byte: &u8, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let hex = format!("0x{}", hex::encode([*byte]));
    serializer.serialize_str(&hex)
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    let bytes = deserializer.deserialize_str(PrefixedHexVisitor)?;
    if bytes.len() != 1 {
        return Err(D::Error::custom(format!("expected 1 byte for u8, got {}", bytes.len())));
    }
    Ok(bytes[0])
}

#[cfg(test)]
mod test {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    struct Wrapper {
        #[serde(with = "super")]
        val: u8,
    }

    #[test]
    fn encoding() {
        assert_eq!(&serde_json::to_string(&Wrapper { val: 0 }).unwrap(), "\"0x00\"");
        assert_eq!(&serde_json::to_string(&Wrapper { val: 109 }).unwrap(), "\"0x6d\"");
        assert_eq!(&serde_json::to_string(&Wrapper { val: u8::MAX }).unwrap(), "\"0xff\"");
    }

    #[test]
    fn decoding() {
        assert_eq!(serde_json::from_str::<Wrapper>("\"0x00\"").unwrap(), Wrapper { val: 0 },);
        assert_eq!(serde_json::from_str::<Wrapper>("\"0x6d\"").unwrap(), Wrapper { val: 109 },);
        assert_eq!(serde_json::from_str::<Wrapper>("\"0xff\"").unwrap(), Wrapper { val: u8::MAX },);

        // Require 0x.
        serde_json::from_str::<Wrapper>("\"ff\"").unwrap_err();
        // Wrong length.
        serde_json::from_str::<Wrapper>("\"0xfff\"").unwrap_err();
    }
}
