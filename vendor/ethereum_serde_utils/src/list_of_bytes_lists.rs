//! Formats `Vec<u64>` using quotes.
//!
//! E.g., `vec![0, 1, 2]` serializes as `["0", "1", "2"]`.
//!
//! Quotes can be optional during decoding.

use serde::{Deserializer, Serializer, de, ser::SerializeSeq};

use crate::hex;

#[derive(Debug)]
pub struct ListOfBytesListVisitor;
impl<'a> serde::de::Visitor<'a> for ListOfBytesListVisitor {
    type Value = Vec<Vec<u8>>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "a list of 0x-prefixed byte lists")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'a>,
    {
        let mut vec = vec![];

        while let Some(val) = seq.next_element::<String>()? {
            vec.push(hex::decode(&val).map_err(de::Error::custom)?);
        }

        Ok(vec)
    }
}

pub fn serialize<S>(value: &[Vec<u8>], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut seq = serializer.serialize_seq(Some(value.len()))?;
    for val in value {
        seq.serialize_element(&hex::encode(val))?;
    }
    seq.end()
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(ListOfBytesListVisitor)
}

#[cfg(test)]
mod test {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    struct Wrapper {
        #[serde(with = "super")]
        val: Vec<Vec<u8>>,
    }

    #[test]
    fn encoding() {
        assert_eq!(&serde_json::to_string(&Wrapper { val: vec![vec![0]] }).unwrap(), "[\"0x00\"]");
        assert_eq!(
            &serde_json::to_string(&Wrapper { val: vec![vec![0, 1, 2]] }).unwrap(),
            "[\"0x000102\"]"
        );
        assert_eq!(
            &serde_json::to_string(&Wrapper { val: vec![vec![0], vec![0, 1, 2]] }).unwrap(),
            "[\"0x00\",\"0x000102\"]"
        );
    }

    #[test]
    fn decoding() {
        assert_eq!(
            serde_json::from_str::<Wrapper>("[\"0x00\"]").unwrap(),
            Wrapper { val: vec![vec![0]] },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>("[\"0x000102\"]").unwrap(),
            Wrapper { val: vec![vec![0, 1, 2]] },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>("[\"0x00\",\"0x000102\"]").unwrap(),
            Wrapper { val: vec![vec![0], vec![0, 1, 2]] },
        );
    }
}
