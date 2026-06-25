use revm::precompile::PrecompileId;
use serde::{Deserialize, Serialize};

/// Typed precompile target for load test configuration.
///
/// Deserializes from a `target` string field with optional precompile-specific
/// parameters (e.g. `rounds` for blake2f).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum PrecompileTarget {
    /// ECDSA public key recovery (`ecrecover`, address `0x01`).
    Ecrecover,
    /// SHA-256 hash (`sha256`, address `0x02`).
    Sha256,
    /// RIPEMD-160 hash (`ripemd160`, address `0x03`).
    Ripemd160,
    /// Identity / data copy (`identity`, address `0x04`).
    Identity,
    /// Modular exponentiation (`modexp`, address `0x05`).
    Modexp,
    /// BN254 elliptic curve addition (`bn254_add`, address `0x06`).
    Bn254Add,
    /// BN254 scalar multiplication (`bn254_mul`, address `0x07`).
    Bn254Mul,
    /// BN254 pairing check (`bn254_pairing`, address `0x08`).
    Bn254Pairing,
    /// `BLAKE2f` compression (`blake2f`, address `0x09`).
    Blake2f {
        /// Fixed number of compression rounds. Random if `None`.
        #[serde(default)]
        rounds: Option<u32>,
    },
    /// KZG point evaluation (`kzg_point_evaluation`, address `0x0a`).
    #[serde(rename = "kzg_point_evaluation")]
    KzgPointEvaluation,
}

impl PrecompileTarget {
    /// Converts to the corresponding `revm` [`PrecompileId`].
    pub const fn to_precompile_id(&self) -> PrecompileId {
        match self {
            Self::Ecrecover => PrecompileId::EcRec,
            Self::Sha256 => PrecompileId::Sha256,
            Self::Ripemd160 => PrecompileId::Ripemd160,
            Self::Identity => PrecompileId::Identity,
            Self::Modexp => PrecompileId::ModExp,
            Self::Bn254Add => PrecompileId::Bn254Add,
            Self::Bn254Mul => PrecompileId::Bn254Mul,
            Self::Bn254Pairing => PrecompileId::Bn254Pairing,
            Self::Blake2f { .. } => PrecompileId::Blake2F,
            Self::KzgPointEvaluation => PrecompileId::KzgPointEvaluation,
        }
    }

    /// Returns the fixed blake2f round count, if configured.
    pub const fn blake2f_rounds(&self) -> Option<u32> {
        match self {
            Self::Blake2f { rounds } => *rounds,
            _ => None,
        }
    }
}
