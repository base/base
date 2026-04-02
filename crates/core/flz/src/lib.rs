#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

mod flz;
pub use flz::{
    L1_COST_FASTLZ_COEF, L1_COST_INTERCEPT, MIN_TX_SIZE_SCALED, NON_ZERO_BYTE_COST, ZERO_BYTE_COST,
    data_gas_fjord, flz_compress_len, tx_estimated_size_fjord, tx_estimated_size_fjord_bytes,
};
