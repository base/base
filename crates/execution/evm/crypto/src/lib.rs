#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[macro_use]
#[cfg(not(feature = "std"))]
extern crate alloc as std;

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

pub use base_execution_evm_primitives as primitives;
pub use id::PrecompileId;
pub use interface::*;

// silence arkworks lint as bn impl will be used as default if both are enabled.
cfg_if::cfg_if! {
    if #[cfg(feature = "bn")]{
        use ark_bn254_0_6_0 as _;
        use ark_ff as _;
        use ark_ec as _;
        use ark_serialize as _;
    }
}

use arrayref as _;

// silence arkworks-bls12-381 lint as blst will be used as default if both are enabled.
cfg_if::cfg_if! {
    if #[cfg(feature = "blst")]{
        use ark_bls12_381 as _;
        use ark_ff as _;
        use ark_ec as _;
        use ark_serialize as _;
    }
}

// silence aurora-engine-modexp if gmp is enabled

#[cfg(feature = "gmp")]
use aurora_engine_modexp as _;
// silence p256 lint as aws-lc-rs will be used if both are enabled.

#[cfg(feature = "p256-aws-lc-rs")]
use p256 as _;

mod sets;
pub use sets::{
    Precompile, PrecompileSpecId, Precompiles, calc_linear_cost, init_precompiles, u64_to_address,
};
