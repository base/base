//! Formats `Vec<u8>` as a 0x-prefixed hex string.
//!
//! E.g., `vec![0, 1, 2, 3]` serializes as `"0x00010203"`.

use serde::{Deserializer, Serializer};

use crate::hex::PrefixedHexVisitor;

pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut hex_string: String = "0x".to_string();
    hex_string.push_str(&hex::encode(bytes));

    serializer.serialize_str(&hex_string)
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_str(PrefixedHexVisitor)
}

#[cfg(test)]
mod test {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    struct Wrapper {
        #[serde(with = "super")]
        val: Vec<u8>,
    }

    #[test]
    fn encoding() {
        assert_eq!(&serde_json::to_string(&Wrapper { val: vec![0] }).unwrap(), "\"0x00\"");
        assert_eq!(&serde_json::to_string(&Wrapper { val: vec![0, 1] }).unwrap(), "\"0x0001\"");
        assert_eq!(
            &serde_json::to_string(&Wrapper { val: vec![0, 1, 2, 3] }).unwrap(),
            "\"0x00010203\""
        );
    }

    #[test]
    fn decoding() {
        assert_eq!(serde_json::from_str::<Wrapper>("\"0x00\"").unwrap(), Wrapper { val: vec![0] },);
        assert_eq!(
            serde_json::from_str::<Wrapper>("\"0x0001\"").unwrap(),
            Wrapper { val: vec![0, 1] },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>("\"0x00010203\"").unwrap(),
            Wrapper { val: vec![0, 1, 2, 3] },
        );
    }
}
