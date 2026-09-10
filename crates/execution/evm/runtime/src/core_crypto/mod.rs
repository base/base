//! Revm Precompiles - Ethereum compatible precompiled contracts.

#[cfg_attr(
    all(any(target_arch = "x86", target_arch = "x86_64"), target_feature = "avx2"),
    expect(unreachable_code)
)]
pub mod blake2;
pub mod bls12_381;
pub mod bls12_381_const;
pub mod bls12_381_utils;
pub mod bn254;
pub mod hash;
mod id;
pub mod identity;
pub mod interface;
pub mod kzg_point_evaluation;
pub mod modexp;
pub mod secp256k1;
pub mod secp256r1;
pub mod utilities;

use arrayref as _;
pub use id::PrecompileId;
pub use interface::*;

// silence arkworks-bls12-381 lint as blst will be used as default if both are enabled.
cfg_if::cfg_if! {
    if #[cfg(feature = "blst")]{
        use ark_bls12_381 as _;
        use ark_ff as _;
        use ark_ec as _;
        use ark_serialize as _;
    }
}

// silence p256 lint as aws-lc-rs will be used if both are enabled.

#[cfg(feature = "p256-aws-lc-rs")]
use p256 as _;

mod sets;
pub use sets::{
    Precompile, PrecompileSpecId, Precompiles, calc_linear_cost, init_precompiles, u64_to_address,
};

pub(crate) use crate::eth_precompile_fn;
