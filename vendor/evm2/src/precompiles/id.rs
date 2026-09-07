use alloc::borrow::Cow;
use core::fmt;

macro_rules! precompile_ids {
    ($($tokens:tt)*) => {
        precompile_ids_find_last! { [] $($tokens)* }
    };
}

macro_rules! precompile_ids_find_last {
    ([$($variants:tt)*] #[$last_doc:meta] $last_variant:ident = $last_name:literal;) => {
        precompile_ids_impl! { [$($variants)*] #[$last_doc] $last_variant = $last_name; }
    };
    ([$($variants:tt)*] #[$doc:meta] $variant:ident = $name:literal; $($rest:tt)+) => {
        precompile_ids_find_last! { [$($variants)* #[$doc] $variant = $name;] $($rest)+ }
    };
}

macro_rules! precompile_ids_impl {
    (
        [#[$first_doc:meta] $first_variant:ident = $first_name:literal; $(#[$doc:meta] $variant:ident = $name:literal;)*]
        #[$last_doc:meta] $last_variant:ident = $last_name:literal;
    ) => {
        /// Unique precompile identifier.
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub enum PrecompileId {
            #[$first_doc]
            $first_variant,
            $(
                #[$doc]
                $variant,
            )*
            #[$last_doc]
            $last_variant,
            /// Custom precompile identifier.
            Custom(Cow<'static, str>),
        }

        impl PrecompileId {
            /// Creates a new custom precompile ID.
            #[inline]
            pub const fn custom(id: &'static str) -> Self {
                Self::Custom(Cow::Borrowed(id))
            }

            /// Returns the name of the precompile as defined in EIP-7910.
            #[inline]
            pub fn name(&self) -> &str {
                match self {
                    Self::$first_variant => $first_name,
                    $(
                        Self::$variant => $name,
                    )*
                    Self::$last_variant => $last_name,
                    Self::Custom(id) => id.as_ref(),
                }
            }
        }

        impl fmt::Display for PrecompileId {
            #[inline]
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.name())
            }
        }
    };
}

precompile_ids! {
    /// Elliptic curve digital signature algorithm (ECDSA) public key recovery function.
    EcRec = "ECREC";
    /// SHA2-256 hash function.
    Sha256 = "SHA256";
    /// RIPEMD-160 hash function.
    Ripemd160 = "RIPEMD160";
    /// Identity precompile.
    Identity = "ID";
    /// Arbitrary-precision exponentiation under modulo.
    ModExp = "MODEXP";
    /// Point addition (ADD) on the elliptic curve 'alt_bn128'.
    Bn254Add = "BN254_ADD";
    /// Scalar multiplication (MUL) on the elliptic curve 'alt_bn128'.
    Bn254Mul = "BN254_MUL";
    /// Bilinear function on groups on the elliptic curve 'alt_bn128'.
    Bn254Pairing = "BN254_PAIRING";
    /// Compression function F used in the BLAKE2 cryptographic hashing algorithm.
    Blake2F = "BLAKE2F";
    /// Verify p(z) = y given commitment that corresponds to the polynomial p(x) and a KZG proof.
    KzgPointEvaluation = "KZG_POINT_EVALUATION";
    /// Point addition in G1 (curve over base prime field).
    Bls12G1Add = "BLS12_G1ADD";
    /// Multi-scalar-multiplication (MSM) in G1 (curve over base prime field).
    Bls12G1Msm = "BLS12_G1MSM";
    /// Point addition in G2 (curve over quadratic extension of the base prime field).
    Bls12G2Add = "BLS12_G2ADD";
    /// Multi-scalar-multiplication (MSM) in G2 (curve over quadratic extension of the base prime field).
    Bls12G2Msm = "BLS12_G2MSM";
    /// Pairing operations between a set of pairs of (G1, G2) points.
    Bls12Pairing = "BLS12_PAIRING_CHECK";
    /// Base field element mapping into the G1 point.
    Bls12MapFpToGp1 = "BLS12_MAP_FP_TO_G1";
    /// Extension field element mapping into the G2 point.
    Bls12MapFp2ToGp2 = "BLS12_MAP_FP2_TO_G2";
    /// ECDSA signature verification over the secp256r1 elliptic curve.
    P256Verify = "P256VERIFY";
}
