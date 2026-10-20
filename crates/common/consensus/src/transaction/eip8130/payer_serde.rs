//! JSON form of an EIP-8130 `payer`.

use alloc::string::String;

use alloy_primitives::Address;
use serde::{Deserialize, Deserializer, de};

use super::Eip8130Constants;

/// Deserializes an optional EIP-8130 `payer`.
///
/// A payer is a 20-byte address. Open payer mode is the zero address, written
/// either in full or as a short all-zero hex string such as `"0x00"`, matching
/// its one-byte wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eip8130PayerSerde;

impl Eip8130PayerSerde {
    /// Deserializes `null` as no payer, a full address as that payer, and a
    /// short all-zero hex string as [`Eip8130Constants::OPEN_PAYER`].
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Address>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Some(raw) = Option::<String>::deserialize(deserializer)? else {
            return Ok(None);
        };
        let digits = raw
            .strip_prefix("0x")
            .or_else(|| raw.strip_prefix("0X"))
            .ok_or_else(|| de::Error::custom("payer must be 0x-prefixed hex"))?;
        if !digits.is_empty()
            && digits.len() < 2 * Address::len_bytes()
            && digits.bytes().all(|digit| digit == b'0')
        {
            return Ok(Some(Eip8130Constants::OPEN_PAYER));
        }
        raw.parse::<Address>().map(Some).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Payer {
        #[serde(default, deserialize_with = "Eip8130PayerSerde::deserialize")]
        payer: Option<Address>,
    }

    fn parse(json: &str) -> Result<Option<Address>, serde_json::Error> {
        serde_json::from_str::<Payer>(json).map(|parsed| parsed.payer)
    }

    #[test]
    fn open_payer_accepts_short_and_full_zero() {
        for zero in ["0x00", "0x0", "0x0000000000000000000000000000000000000000"] {
            assert_eq!(
                parse(&format!(r#"{{"payer":"{zero}"}}"#)).unwrap(),
                Some(Eip8130Constants::OPEN_PAYER)
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
}
