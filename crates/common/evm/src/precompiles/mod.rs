//! Contains Base specific precompiles.

mod provider;
pub use provider::BasePrecompiles;

mod bn254_pair;
pub use bn254_pair::{
    GRANITE, GRANITE_MAX_INPUT_SIZE, JOVIAN, JOVIAN_MAX_INPUT_SIZE, run_pair_granite,
    run_pair_jovian,
};

mod bls12_381;
pub use bls12_381::{
    ISTHMUS_G1_MSM, ISTHMUS_G1_MSM_MAX_INPUT_SIZE, ISTHMUS_G2_MSM, ISTHMUS_G2_MSM_MAX_INPUT_SIZE,
    ISTHMUS_PAIRING, ISTHMUS_PAIRING_MAX_INPUT_SIZE, JOVIAN_G1_MSM, JOVIAN_G1_MSM_MAX_INPUT_SIZE,
    JOVIAN_G2_MSM, JOVIAN_G2_MSM_MAX_INPUT_SIZE, JOVIAN_PAIRING, JOVIAN_PAIRING_MAX_INPUT_SIZE,
    run_g1_msm_isthmus, run_g1_msm_jovian, run_g2_msm_isthmus, run_g2_msm_jovian,
    run_pairing_isthmus, run_pairing_jovian,
};
