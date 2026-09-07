use alloc::{borrow::Cow, string::String};

use alloy_primitives::Bytes;
use thiserror::Error;

use crate::{AnyError, evm::precompile::PrecompileOutput};

/// A precompile operation result type for individual precompile functions.
pub type PrecompileResult = Result<PrecompileOutput, PrecompileError>;

/// Non-fatal halt reasons for precompiles.
///
/// These represent conditions that halt precompile execution but do not abort
/// the entire EVM transaction.
#[derive(Clone, Debug, Error, PartialEq, Eq, Hash)]
pub enum PrecompileHalt {
    /// out of gas is the main error. Others are here just for completeness
    #[error("out of gas")]
    OutOfGas,
    /// Blake2 errors
    #[error("wrong input length for blake2")]
    Blake2WrongLength,
    /// Blake2 wrong final indicator flag
    #[error("wrong final indicator flag for blake2")]
    Blake2WrongFinalIndicatorFlag,
    /// Modexp errors
    #[error("modexp exp overflow")]
    ModexpExpOverflow,
    /// Modexp base overflow
    #[error("modexp base overflow")]
    ModexpBaseOverflow,
    /// Modexp mod overflow
    #[error("modexp mod overflow")]
    ModexpModOverflow,
    /// Modexp limit all input sizes.
    #[error("Modexp limit all input sizes.")]
    ModexpEip7823LimitSize,
    /// Bn254 errors
    #[error("field point not a member of bn254 curve")]
    Bn254FieldPointNotAMember,
    /// Bn254 affine g failed to create
    #[error("failed to create affine g point for bn254 curve")]
    Bn254AffineGFailedToCreate,
    /// Bn254 pair length
    #[error("bn254 invalid pair length")]
    Bn254PairLength,
    // Blob errors
    /// The input length is not exactly 192 bytes
    #[error("invalid blob input length")]
    BlobInvalidInputLength,
    /// The commitment does not match the versioned hash
    #[error("mismatched blob version")]
    BlobMismatchedVersion,
    /// The proof verification failed
    #[error("verifying blob kzg proof failed")]
    BlobVerifyKzgProofFailed,
    /// Non-canonical field element
    #[error("non-canonical field element")]
    NonCanonicalFp,
    /// BLS12-381 G1 point not on curve
    #[error("bls12-381 g1 point not on curve")]
    Bls12381G1NotOnCurve,
    /// BLS12-381 G1 point not in correct subgroup
    #[error("bls12-381 g1 point not in correct subgroup")]
    Bls12381G1NotInSubgroup,
    /// BLS12-381 G2 point not on curve
    #[error("bls12-381 g2 point not on curve")]
    Bls12381G2NotOnCurve,
    /// BLS12-381 G2 point not in correct subgroup
    #[error("bls12-381 g2 point not in correct subgroup")]
    Bls12381G2NotInSubgroup,
    /// BLS12-381 scalar input length error
    #[error("bls12-381 scalar input length error")]
    Bls12381ScalarInputLength,
    /// BLS12-381 G1 add input length error
    #[error("bls12-381 g1 add input length error")]
    Bls12381G1AddInputLength,
    /// BLS12-381 G1 msm input length error
    #[error("bls12-381 g1 msm input length error")]
    Bls12381G1MsmInputLength,
    /// BLS12-381 G2 add input length error
    #[error("bls12-381 g2 add input length error")]
    Bls12381G2AddInputLength,
    /// BLS12-381 G2 msm input length error
    #[error("bls12-381 g2 msm input length error")]
    Bls12381G2MsmInputLength,
    /// BLS12-381 pairing input length error
    #[error("bls12-381 pairing input length error")]
    Bls12381PairingInputLength,
    /// BLS12-381 map fp to g1 input length error
    #[error("bls12-381 map fp to g1 input length error")]
    Bls12381MapFpToG1InputLength,
    /// BLS12-381 map fp2 to g2 input length error
    #[error("bls12-381 map fp2 to g2 input length error")]
    Bls12381MapFp2ToG2InputLength,
    /// BLS12-381 padding error
    #[error("bls12-381 fp 64 top bytes of input are not zero")]
    Bls12381FpPaddingInvalid,
    /// BLS12-381 fp padding length error
    #[error("bls12-381 fp padding length error")]
    Bls12381FpPaddingLength,
    /// BLS12-381 g1 padding length error
    #[error("bls12-381 g1 padding length error")]
    Bls12381G1PaddingLength,
    /// BLS12-381 g2 padding length error
    #[error("bls12-381 g2 padding length error")]
    Bls12381G2PaddingLength,
    /// KZG invalid G1 point
    #[error("kzg invalid g1 point")]
    KzgInvalidG1Point,
    /// KZG G1 point not on curve
    #[error("kzg g1 point not on curve")]
    KzgG1PointNotOnCurve,
    /// KZG G1 point not in correct subgroup
    #[error("kzg g1 point not in correct subgroup")]
    KzgG1PointNotInSubgroup,
    /// KZG input length error
    #[error("kzg invalid input length")]
    KzgInvalidInputLength,
    /// secp256k1 ecrecover failed
    #[error("secp256k1 signature recovery failed")]
    Secp256k1RecoverFailed,
    /// Catch-all variant for precompile halt reasons without a dedicated variant.
    #[error("{0}")]
    Other(Cow<'static, str>),
}

impl From<crate::interpreter::InstrStop> for PrecompileError {
    #[inline]
    fn from(x: crate::interpreter::InstrStop) -> Self {
        debug_assert!(x.is_halt());
        if x.is_fatal() {
            return Self::Fatal("fatal external error".into());
        }
        Self::Halt(PrecompileHalt::OutOfGas)
    }
}

/// Precompile error type.
#[derive(Clone, Debug, Error)]
pub enum PrecompileError {
    /// Precompile reverted.
    #[error("revert")]
    Revert(Bytes),
    /// Precompile halted with a non-fatal reason.
    #[error("{0}")]
    Halt(#[from] PrecompileHalt),
    /// Unrecoverable error that halts EVM execution.
    #[error("fatal: {0}")]
    Fatal(AnyError),
}

impl PrecompileError {
    /// Creates a fatal error.
    #[inline]
    pub fn fatal(err: impl core::error::Error + Send + Sync + 'static) -> Self {
        Self::Fatal(AnyError::new(err))
    }

    /// Returns `true` if the error is fatal.
    pub const fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal(_))
    }

    /// Returns the halt reason, if this is a halt error.
    #[inline]
    pub const fn as_halt(&self) -> Option<&PrecompileHalt> {
        match self {
            Self::Halt(halt) => Some(halt),
            _ => None,
        }
    }

    /// Returns `true` if this error is an out-of-gas halt.
    #[inline]
    pub const fn is_oog(&self) -> bool {
        matches!(self, Self::Halt(PrecompileHalt::OutOfGas))
    }
}

impl From<AnyError> for PrecompileError {
    #[inline]
    fn from(err: AnyError) -> Self {
        Self::Fatal(err)
    }
}

impl From<String> for PrecompileError {
    #[inline]
    fn from(err: String) -> Self {
        Self::Fatal(err.into())
    }
}

impl From<&'static str> for PrecompileError {
    #[inline]
    fn from(err: &'static str) -> Self {
        Self::Fatal(err.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::InstrStop;

    #[test]
    fn fatal_instr_stop_becomes_fatal_error() {
        assert!(PrecompileError::from(InstrStop::FatalExternalError).is_fatal());
        assert!(PrecompileError::from(InstrStop::FatalPrecompileError).is_fatal());
    }

    #[test]
    fn non_fatal_instr_stop_remains_oog_precompile_halt() {
        core::assert_matches!(
            PrecompileError::from(InstrStop::OutOfGas),
            PrecompileError::Halt(PrecompileHalt::OutOfGas)
        );
    }
}
