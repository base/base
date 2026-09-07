use alloy_primitives::U256;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub fn serialize<S>(num: &U256, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    num.to_string().serialize(serializer)
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<U256, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    U256::from_str_radix(&s, 10).map_err(|e| de::Error::custom(format!("Invalid U256 string: {e}")))
}

#[cfg(test)]
mod test {
    use alloy_primitives::U256;
    use serde::{Deserialize, Serialize};
    use serde_json;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    struct Wrapper {
        #[serde(with = "super")]
        val: U256,
    }

    #[test]
    fn encoding() {
        assert_eq!(&serde_json::to_string(&Wrapper { val: U256::from(0) }).unwrap(), "\"0\"");
        assert_eq!(&serde_json::to_string(&Wrapper { val: U256::from(1) }).unwrap(), "\"1\"");
        assert_eq!(&serde_json::to_string(&Wrapper { val: U256::from(256) }).unwrap(), "\"256\"");
        assert_eq!(&serde_json::to_string(&Wrapper { val: U256::from(65) }).unwrap(), "\"65\"");
        assert_eq!(&serde_json::to_string(&Wrapper { val: U256::from(1024) }).unwrap(), "\"1024\"");
        assert_eq!(
            &serde_json::to_string(&Wrapper { val: U256::MAX - U256::from(1) }).unwrap(),
            "\"115792089237316195423570985008687907853269984665640564039457584007913129639934\""
        );
        assert_eq!(
            &serde_json::to_string(&Wrapper { val: U256::MAX }).unwrap(),
            "\"115792089237316195423570985008687907853269984665640564039457584007913129639935\""
        );
    }

    #[test]
    fn decoding() {
        assert_eq!(
            serde_json::from_str::<Wrapper>("\"0\"").unwrap(),
            Wrapper { val: U256::from(0) },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>("\"65\"").unwrap(),
            Wrapper { val: U256::from(65) },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>("\"1024\"").unwrap(),
            Wrapper { val: U256::from(1024) },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>(
                "\"115792089237316195423570985008687907853269984665640564039457584007913129639934\""
            )
            .unwrap(),
            Wrapper { val: U256::MAX - U256::from(1) },
        );
        assert_eq!(
            serde_json::from_str::<Wrapper>(
                "\"115792089237316195423570985008687907853269984665640564039457584007913129639935\""
            )
            .unwrap(),
            Wrapper { val: U256::MAX },
        );
        serde_json::from_str::<Wrapper>("\"0x0\"").unwrap_err();
        serde_json::from_str::<Wrapper>("\"0x0400\"").unwrap_err();
        serde_json::from_str::<Wrapper>("\"-1\"").unwrap_err();
        serde_json::from_str::<Wrapper>("\"ff\"").unwrap_err();
    }
}
