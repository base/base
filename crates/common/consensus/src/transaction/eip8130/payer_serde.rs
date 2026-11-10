//! JSON form of an EIP-8130 `payer`.

use alloc::string::String;

use alloy_primitives::Address;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::Eip8130Constants;

/// Serializes and deserializes an optional EIP-8130 `payer` in the same form
/// as its wire encoding.
///
/// `null` (or an absent field) is self-pay, `"0x00"` is open payer mode, and a
/// 20-byte address is that named payer. The zero address is rejected: open
/// mode is only `"0x00"`, and a named zero-address payer could never
/// authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eip8130PayerSerde;

impl Eip8130PayerSerde {
    /// JSON form of [`Eip8130Constants::OPEN_PAYER`], matching its one-byte
    /// wire encoding.
    pub const OPEN_PAYER_JSON: &'static str = "0x00";

    /// Serializes `None` as `null`, [`Eip8130Constants::OPEN_PAYER`] as
    /// [`Self::OPEN_PAYER_JSON`], and any other address as that address.
    pub fn serialize<S>(payer: &Option<Address>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match payer {
            Some(Eip8130Constants::OPEN_PAYER) => serializer.serialize_str(Self::OPEN_PAYER_JSON),
            payer => payer.serialize(serializer),
        }
    }

    /// Deserializes `null` as no payer, [`Self::OPEN_PAYER_JSON`] as
    /// [`Eip8130Constants::OPEN_PAYER`], and a non-zero address as that payer.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Address>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Some(raw) = Option::<String>::deserialize(deserializer)? else {
            return Ok(None);
        };
        if raw == Self::OPEN_PAYER_JSON {
            return Ok(Some(Eip8130Constants::OPEN_PAYER));
        }
        let payer = raw.parse::<Address>().map_err(de::Error::custom)?;
        if payer == Eip8130Constants::OPEN_PAYER {
            return Err(de::Error::custom("open payer mode is \"0x00\", not the zero address"));
        }
        Ok(Some(payer))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Payer {
        #[serde(
            default,
            serialize_with = "Eip8130PayerSerde::serialize",
            deserialize_with = "Eip8130PayerSerde::deserialize"
        )]
        payer: Option<Address>,
    }

    fn parse(json: &str) -> Result<Option<Address>, serde_json::Error> {
        serde_json::from_str::<Payer>(json).map(|parsed| parsed.payer)
    }

    #[test]
    fn open_payer_is_only_the_one_byte_form() {
        assert_eq!(parse(r#"{"payer":"0x00"}"#).unwrap(), Some(Eip8130Constants::OPEN_PAYER));
        for other in ["0x0", "0x000", "0X00", "0x0000000000000000000000000000000000000000"] {
            assert!(
                parse(&format!(r#"{{"payer":"{other}"}}"#)).is_err(),
                "{other} must be rejected"
            );
        }
    }

    #[test]
    fn named_absent_and_malformed_payers() {
        let named = address!("0x00000000000000000000000000000000000000b2");
        assert_eq!(parse(&format!(r#"{{"payer":"{named}"}}"#)).unwrap(), Some(named));
        assert_eq!(parse(r#"{"payer":null}"#).unwrap(), None);
        assert_eq!(parse("{}").unwrap(), None);
        assert!(parse(r#"{"payer":"0x01"}"#).is_err(), "a short non-zero payer is not an address");
        assert!(parse(r#"{"payer":"0x"}"#).is_err());
    }

    #[test]
    fn every_payer_round_trips_through_its_canonical_form() {
        let named = address!("0x00000000000000000000000000000000000000b2");
        for (payer, json) in [
            (None, serde_json::Value::Null),
            (Some(Eip8130Constants::OPEN_PAYER), "0x00".into()),
            (Some(named), serde_json::to_value(named).unwrap()),
        ] {
            let encoded = serde_json::to_value(Payer { payer }).unwrap();
            assert_eq!(encoded["payer"], json);
            assert_eq!(serde_json::from_value::<Payer>(encoded).unwrap().payer, payer);
        }
    }
}
