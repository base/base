//! Serialize a datatype as a JSON-blob within a single string.
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{DeserializeOwned, Error as _},
    ser::Error as _,
};

/// Serialize as a JSON object within a string.
pub fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize,
{
    serializer.serialize_str(&serde_json::to_string(value).map_err(S::Error::custom)?)
}

/// Deserialize a JSON object embedded in a string.
pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let json_str = String::deserialize(deserializer)?;
    serde_json::from_str(&json_str).map_err(D::Error::custom)
}

#[cfg(test)]
mod test {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Wrapper {
        val_1: String,
        val_2: u8,
        val_3: Vec<u8>,
        val_4: Option<u32>,
    }

    #[test]
    fn encoding() {
        assert_eq!(
            &serde_json::to_string(&Wrapper {
                val_1: "Test".to_string(),
                val_2: 5,
                val_3: vec![9],
                val_4: Some(10)
            })
            .unwrap(),
            "{\"val_1\":\"Test\",\"val_2\":5,\"val_3\":[9],\"val_4\":10}"
        );
        assert_eq!(
            &serde_json::to_string(&Wrapper {
                val_1: "Test".to_string(),
                val_2: 5,
                val_3: vec![9],
                val_4: None
            })
            .unwrap(),
            "{\"val_1\":\"Test\",\"val_2\":5,\"val_3\":[9],\"val_4\":null}"
        );
    }

    #[test]
    fn decoding() {
        assert_eq!(
            serde_json::from_str::<Wrapper>(
                "{\"val_1\":\"Test\",\"val_2\":5,\"val_3\":[9],\"val_4\":10}"
            )
            .unwrap(),
            Wrapper { val_1: "Test".to_string(), val_2: 5, val_3: vec![9], val_4: Some(10) },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>(
                "{\"val_1\":\"Test\",\"val_2\":5,\"val_3\":[9],\"val_4\":null}"
            )
            .unwrap(),
            Wrapper { val_1: "Test".to_string(), val_2: 5, val_3: vec![9], val_4: None },
        );

        // Violating type constraints.
        serde_json::from_str::<Wrapper>("{\"val_1\":1,\"val_2\":5,\"val_3\":[9],\"val_4\":null}")
            .unwrap_err();
        serde_json::from_str::<Wrapper>("{\"val_1\":\"Test\",\"val_2\":5,\"val_4\":null}")
            .unwrap_err();
    }
}
