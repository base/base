//! Contains the Base consensus-layer ENR Type.

use alloy_rlp::{Decodable, Encodable};
use discv5::Enr;
use unsigned_varint::{decode, encode};

/// Validates the [`Enr`] for Base.
#[derive(Debug, derive_more::Display, Clone, Default, PartialEq, Eq)]
pub enum EnrValidation {
    /// Conversion error.
    #[display("Conversion error: {_0}")]
    ConversionError(BaseEnrError),
    /// Invalid Chain ID.
    #[display("Invalid Chain ID: {_0}")]
    InvalidChainId(u64),
    /// Valid ENR.
    #[default]
    #[display("Valid ENR")]
    Valid,
}

impl EnrValidation {
    /// Validates the [`Enr`] for Base.
    pub fn validate(enr: &Enr, chain_id: u64) -> Self {
        let base_enr = match BaseEnr::try_from(enr) {
            Ok(base_enr) => base_enr,
            Err(e) => return Self::ConversionError(e),
        };

        if base_enr.chain_id != chain_id {
            return Self::InvalidChainId(base_enr.chain_id);
        }

        Self::Valid
    }

    /// Returns `true` if the ENR is valid.
    pub const fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }

    /// Returns `true` if the ENR is invalid.
    pub const fn is_invalid(&self) -> bool {
        !self.is_valid()
    }
}

/// The unique L2 network identifier
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct BaseEnr {
    /// Chain ID
    pub chain_id: u64,
    /// The version. Always set to 0.
    pub version: u64,
}

/// The error type that can be returned when trying to convert an [`Enr`] to a [`BaseEnr`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum BaseEnrError {
    /// Missing Base ENR key.
    #[error("Missing Base ENR key")]
    MissingKey,
    /// Failed to decode the Base ENR Value.
    #[error("Failed to decode the Base ENR Value: {0}")]
    DecodeError(String),
    /// Invalid version.
    #[error("Invalid version: {0}")]
    InvalidVersion(u64),
}

impl TryFrom<&Enr> for BaseEnr {
    type Error = BaseEnrError;
    fn try_from(enr: &Enr) -> Result<Self, Self::Error> {
        let Some(mut opstack) = enr.get_raw_rlp(Self::OP_CL_KEY) else {
            return Err(BaseEnrError::MissingKey);
        };
        let base_enr =
            Self::decode(&mut opstack).map_err(|e| BaseEnrError::DecodeError(e.to_string()))?;

        if base_enr.version != 0 {
            return Err(BaseEnrError::InvalidVersion(base_enr.version));
        }

        Ok(base_enr)
    }
}

impl BaseEnr {
    /// The [`Enr`] key literal string for the consensus layer.
    pub const OP_CL_KEY: &str = "opstack";

    /// Constructs a [`BaseEnr`] from a chain id.
    pub const fn from_chain_id(chain_id: u64) -> Self {
        Self { chain_id, version: 0 }
    }
}

impl Encodable for BaseEnr {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        let mut chain_id_buf = encode::u128_buffer();
        let chain_id_slice = encode::u128(self.chain_id as u128, &mut chain_id_buf);

        let mut version_buf = encode::u128_buffer();
        let version_slice = encode::u128(self.version as u128, &mut version_buf);

        let payload = [chain_id_slice, version_slice].concat();
        alloy_primitives::Bytes::from(payload).encode(out);
    }
}

impl Decodable for BaseEnr {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let bytes = alloy_primitives::Bytes::decode(buf)?;
        let (chain_id, rest) = decode::u64(&bytes)
            .map_err(|_| alloy_rlp::Error::Custom("could not decode chain id"))?;
        let (version, _) =
            decode::u64(rest).map_err(|_| alloy_rlp::Error::Custom("could not decode version"))?;
        Ok(Self { chain_id, version })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, bytes};
    use discv5::enr::CombinedKey;

    use super::*;

    #[test]
    #[cfg(feature = "arbitrary")]
    fn roundtrip_base_enr() {
        arbtest::arbtest(|u| {
            let base_enr = BaseEnr::from_chain_id(u.arbitrary()?);
            let bytes = alloy_rlp::encode(base_enr);
            let decoded = BaseEnr::decode(&mut &bytes[..]).unwrap();
            assert_eq!(decoded, base_enr);
            Ok(())
        });
    }

    #[test]
    fn test_enr_validation() {
        let key = CombinedKey::generate_secp256k1();
        let mut enr = Enr::builder().build(&key).unwrap();
        let base_enr = BaseEnr::from_chain_id(8453);
        let mut base_enr_bytes = Vec::new();
        base_enr.encode(&mut base_enr_bytes);
        enr.insert_raw_rlp(BaseEnr::OP_CL_KEY, base_enr_bytes.into(), &key).unwrap();
        assert!(EnrValidation::validate(&enr, 8453).is_valid());
        assert!(EnrValidation::validate(&enr, 84532).is_invalid());
    }

    #[test]
    fn test_enr_validation_invalid_version() {
        let key = CombinedKey::generate_secp256k1();
        let mut enr = Enr::builder().build(&key).unwrap();
        let mut base_enr = BaseEnr::from_chain_id(8453);
        base_enr.version = 1;
        let mut base_enr_bytes = Vec::new();
        base_enr.encode(&mut base_enr_bytes);
        enr.insert_raw_rlp(BaseEnr::OP_CL_KEY, base_enr_bytes.into(), &key).unwrap();
        assert!(EnrValidation::validate(&enr, 8453).is_invalid());
    }

    #[test]
    fn test_base_mainnet_enr() {
        let base_enr = BaseEnr::from_chain_id(8453);
        let bytes = alloy_rlp::encode(base_enr);
        assert_eq!(Bytes::from(bytes.clone()), bytes!("83854200"));
        let decoded = BaseEnr::decode(&mut &bytes[..]).unwrap();
        assert_eq!(decoded, base_enr);
    }
}
