use alloc::{boxed::Box, vec::Vec};
use core::fmt::Debug;

use crate::{
    once_lock::OnceLock,
    precompiles::{
        PrecompileHalt,
        bls12_381::{G1Point, G1PointScalar, G2Point, G2PointScalar},
    },
};

/// Crypto operations trait for precompiles.
pub trait Crypto: Send + Sync + Debug {
    /// Compute SHA-256 hash
    #[inline]
    fn sha256(&self, input: &[u8]) -> [u8; 32] {
        use sha2_0_11_0::Digest;
        let output = sha2_0_11_0::Sha256::digest(input);
        output.into()
    }

    /// Compute RIPEMD-160 hash
    #[inline]
    fn ripemd160(&self, input: &[u8]) -> [u8; 32] {
        use ripemd::Digest;
        let mut hasher = ripemd::Ripemd160::new();
        hasher.update(input);

        let mut output = [0u8; 32];
        output[12..].copy_from_slice(&hasher.finalize());
        output
    }

    /// BN254 elliptic curve addition.
    #[inline]
    fn bn254_g1_add(&self, p1: &[u8], p2: &[u8]) -> Result<[u8; 64], PrecompileHalt> {
        crate::precompiles::bn254::g1_point_add(p1, p2)
    }

    /// BN254 elliptic curve scalar multiplication.
    #[inline]
    fn bn254_g1_mul(&self, point: &[u8], scalar: &[u8]) -> Result<[u8; 64], PrecompileHalt> {
        crate::precompiles::bn254::g1_point_mul(point, scalar)
    }

    /// BN254 pairing check.
    #[inline]
    fn bn254_pairing_check(&self, pairs: &[(&[u8], &[u8])]) -> Result<bool, PrecompileHalt> {
        crate::precompiles::bn254::pairing_check(pairs)
    }

    /// secp256k1 ECDSA signature recovery.
    #[inline]
    fn secp256k1_ecrecover(
        &self,
        sig: &[u8; 64],
        recid: u8,
        msg: &[u8; 32],
    ) -> Result<[u8; 32], PrecompileHalt> {
        crate::precompiles::secp256k1::ecrecover_bytes(sig, recid, msg)
            .ok_or(PrecompileHalt::Secp256k1RecoverFailed)
    }

    /// Modular exponentiation.
    #[inline]
    fn modexp(&self, base: &[u8], exp: &[u8], modulus: &[u8]) -> Result<Vec<u8>, PrecompileHalt> {
        Ok(crate::precompiles::modexp::modexp(base, exp, modulus))
    }

    /// Blake2 compression function.
    #[inline]
    fn blake2_compress(&self, rounds: u32, h: &mut [u64; 8], m: &[u64; 16], t: &[u64; 2], f: bool) {
        crate::precompiles::blake2::compress(rounds, h, m, t, f);
    }

    /// secp256r1 (P-256) signature verification.
    #[inline]
    fn secp256r1_verify_signature(&self, msg: &[u8; 32], sig: &[u8; 64], pk: &[u8; 64]) -> bool {
        crate::precompiles::secp256r1::verify_signature(msg, sig, pk).is_some()
    }

    /// KZG point evaluation.
    #[inline]
    fn verify_kzg_proof(
        &self,
        z: &[u8; 32],
        y: &[u8; 32],
        commitment: &[u8; 48],
        proof: &[u8; 48],
    ) -> Result<(), PrecompileHalt> {
        if !crate::precompiles::kzg_point_evaluation::verify_kzg_proof(commitment, z, y, proof) {
            return Err(PrecompileHalt::BlobVerifyKzgProofFailed);
        }

        Ok(())
    }

    /// BLS12-381 G1 addition (returns 96-byte unpadded G1 point)
    fn bls12_381_g1_add(&self, a: G1Point, b: G1Point) -> Result<[u8; 96], PrecompileHalt> {
        crate::precompiles::bls12_381::crypto_backend::p1_add_affine_bytes(a, b)
    }

    /// BLS12-381 G1 multi-scalar multiplication (returns 96-byte unpadded G1 point)
    fn bls12_381_g1_msm(
        &self,
        pairs: &mut dyn Iterator<Item = Result<G1PointScalar, PrecompileHalt>>,
    ) -> Result<[u8; 96], PrecompileHalt> {
        crate::precompiles::bls12_381::crypto_backend::p1_msm_bytes(pairs)
    }

    /// BLS12-381 G2 addition (returns 192-byte unpadded G2 point)
    fn bls12_381_g2_add(&self, a: G2Point, b: G2Point) -> Result<[u8; 192], PrecompileHalt> {
        crate::precompiles::bls12_381::crypto_backend::p2_add_affine_bytes(a, b)
    }

    /// BLS12-381 G2 multi-scalar multiplication (returns 192-byte unpadded G2 point)
    fn bls12_381_g2_msm(
        &self,
        pairs: &mut dyn Iterator<Item = Result<G2PointScalar, PrecompileHalt>>,
    ) -> Result<[u8; 192], PrecompileHalt> {
        crate::precompiles::bls12_381::crypto_backend::p2_msm_bytes(pairs)
    }

    /// BLS12-381 pairing check.
    fn bls12_381_pairing_check(
        &self,
        pairs: &[(G1Point, G2Point)],
    ) -> Result<bool, PrecompileHalt> {
        crate::precompiles::bls12_381::crypto_backend::pairing_check_bytes(pairs)
    }

    /// BLS12-381 map field element to G1.
    fn bls12_381_fp_to_g1(&self, fp: &[u8; 48]) -> Result<[u8; 96], PrecompileHalt> {
        crate::precompiles::bls12_381::crypto_backend::map_fp_to_g1_bytes(fp)
    }

    /// BLS12-381 map field element to G2.
    fn bls12_381_fp2_to_g2(&self, fp2: ([u8; 48], [u8; 48])) -> Result<[u8; 192], PrecompileHalt> {
        crate::precompiles::bls12_381::crypto_backend::map_fp2_to_g2_bytes(&fp2.0, &fp2.1)
    }
}

/// Default implementation of the Crypto trait using the existing crypto libraries.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultCrypto(());

impl DefaultCrypto {
    /// Creates a new default crypto provider.
    #[inline]
    pub const fn new() -> Self {
        Self(())
    }
}

impl Crypto for DefaultCrypto {}

/// Global crypto provider instance.
static CRYPTO: OnceLock<Box<dyn Crypto>> = OnceLock::new();

/// Install a custom crypto provider globally.
pub fn install_crypto<C: Crypto + 'static>(crypto: C) -> bool {
    CRYPTO.set(Box::new(crypto)).is_ok()
}

/// Get the installed crypto provider, or the default if none is installed.
pub fn crypto() -> &'static dyn Crypto {
    CRYPTO.get_or_init(|| Box::new(DefaultCrypto::new())).as_ref()
}
