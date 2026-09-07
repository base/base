//! Formats `u32` as a 0x-prefixed, little-endian hex string.
//!
//! E.g., `0` serializes as `"0x00000000"`.

use serde::{Deserializer, Serializer};

use crate::bytes_4_hex;

pub fn serialize<S>(num: &u32, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let hex = format!("0x{}", hex::encode(num.to_le_bytes()));
    serializer.serialize_str(&hex)
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    bytes_4_hex::deserialize(deserializer).map(u32::from_le_bytes)
}

#[cfg(test)]
pub mod test {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    struct Wrapper {
        #[serde(with = "super")]
        val: u32,
    }

    #[test]
    fn encoding() {
        assert_eq!(&serde_json::to_string(&Wrapper { val: 0 }).unwrap(), "\"0x00000000\"");
        assert_eq!(&serde_json::to_string(&Wrapper { val: 5 }).unwrap(), "\"0x05000000\"");

        assert_eq!(&serde_json::to_string(&Wrapper { val: u32::MAX }).unwrap(), "\"0xffffffff\"");
    }

    #[test]
    fn decoding() {
        assert_eq!(serde_json::from_str::<Wrapper>("\"0x00000000\"").unwrap(), Wrapper { val: 0 },);
        assert_eq!(serde_json::from_str::<Wrapper>("\"0x05000000\"").unwrap(), Wrapper { val: 5 },);
        assert_eq!(
            serde_json::from_str::<Wrapper>("\"0xffffffff\"").unwrap(),
            Wrapper { val: u32::MAX },
        );

        // Wrong length.
        serde_json::from_str::<Wrapper>("\"0xfffffffff\"").unwrap_err();
        // Requires 0x.
        serde_json::from_str::<Wrapper>("\"00000000\"").unwrap_err();
    }
}
