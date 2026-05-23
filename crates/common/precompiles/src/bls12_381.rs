use alloc::string::ToString;

use revm::precompile::{
    self as precompile, Precompile, PrecompileError, PrecompileId, PrecompileResult,
    bls12_381_const::{G1_MSM_ADDRESS, G2_MSM_ADDRESS, PAIRING_ADDRESS},
    call_eth_precompile,
};

/// Max input size for the BLS12-381 G1 MSM precompile after the Isthmus hardfork.
pub(crate) const ISTHMUS_G1_MSM_MAX_INPUT_SIZE: usize = 513760;
/// Max input size for the BLS12-381 G1 MSM precompile after the Jovian hardfork.
pub const JOVIAN_G1_MSM_MAX_INPUT_SIZE: usize = 288_960;

/// Max input size for the BLS12-381 G2 MSM precompile after the Isthmus hardfork.
pub(crate) const ISTHMUS_G2_MSM_MAX_INPUT_SIZE: usize = 488448;
/// Max input size for the BLS12-381 G2 MSM precompile after the Jovian hardfork.
pub const JOVIAN_G2_MSM_MAX_INPUT_SIZE: usize = 278_784;

/// Max input size for the BLS12-381 pairing precompile after the Isthmus hardfork.
pub(crate) const ISTHMUS_PAIRING_MAX_INPUT_SIZE: usize = 235008;
/// Max input size for the BLS12-381 pairing precompile after the Jovian hardfork.
pub const JOVIAN_PAIRING_MAX_INPUT_SIZE: usize = 156_672;

/// BLS12-381 G1 MSM precompile with Isthmus input limits.
pub(crate) const ISTHMUS_G1_MSM: Precompile =
    Precompile::new(PrecompileId::Bls12G1Msm, G1_MSM_ADDRESS, run_isthmus_g1_msm);

fn run_isthmus_g1_msm(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    if input.len() > ISTHMUS_G1_MSM_MAX_INPUT_SIZE {
        return Err(PrecompileError::Fatal(
            "G1MSM input length too long for Base input size limitation after the Isthmus Hardfork"
                .to_string(),
        ));
    }
    Ok(call_eth_precompile(precompile::bls12_381::g1_msm::g1_msm, input, gas_limit, reservoir))
}

/// BLS12-381 G2 MSM precompile with Isthmus input limits.
pub(crate) const ISTHMUS_G2_MSM: Precompile =
    Precompile::new(PrecompileId::Bls12G2Msm, G2_MSM_ADDRESS, run_isthmus_g2_msm);

fn run_isthmus_g2_msm(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    if input.len() > ISTHMUS_G2_MSM_MAX_INPUT_SIZE {
        return Err(PrecompileError::Fatal(
            "G2MSM input length too long for Base input size limitation".to_string(),
        ));
    }
    Ok(call_eth_precompile(precompile::bls12_381::g2_msm::g2_msm, input, gas_limit, reservoir))
}

/// BLS12-381 pairing precompile with Isthmus input limits.
pub(crate) const ISTHMUS_PAIRING: Precompile =
    Precompile::new(PrecompileId::Bls12Pairing, PAIRING_ADDRESS, run_isthmus_pairing);

fn run_isthmus_pairing(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    if input.len() > ISTHMUS_PAIRING_MAX_INPUT_SIZE {
        return Err(PrecompileError::Fatal(
            "Pairing input length too long for Base input size limitation".to_string(),
        ));
    }
    Ok(call_eth_precompile(precompile::bls12_381::pairing::pairing, input, gas_limit, reservoir))
}

/// BLS12-381 G1 MSM precompile with Jovian input limits.
pub const JOVIAN_G1_MSM: Precompile =
    Precompile::new(PrecompileId::Bls12G1Msm, G1_MSM_ADDRESS, run_jovian_g1_msm);

fn run_jovian_g1_msm(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    if input.len() > JOVIAN_G1_MSM_MAX_INPUT_SIZE {
        return Err(PrecompileError::Fatal(
            "G1MSM input length too long for Base input size limitation after the Jovian Hardfork"
                .to_string(),
        ));
    }
    Ok(call_eth_precompile(precompile::bls12_381::g1_msm::g1_msm, input, gas_limit, reservoir))
}

/// BLS12-381 G2 MSM precompile with Jovian input limits.
pub const JOVIAN_G2_MSM: Precompile =
    Precompile::new(PrecompileId::Bls12G2Msm, G2_MSM_ADDRESS, run_jovian_g2_msm);

fn run_jovian_g2_msm(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    if input.len() > JOVIAN_G2_MSM_MAX_INPUT_SIZE {
        return Err(PrecompileError::Fatal(
            "G2MSM input length too long for Base input size limitation after the Jovian Hardfork"
                .to_string(),
        ));
    }
    Ok(call_eth_precompile(precompile::bls12_381::g2_msm::g2_msm, input, gas_limit, reservoir))
}

/// BLS12-381 pairing precompile with Jovian input limits.
pub const JOVIAN_PAIRING: Precompile =
    Precompile::new(PrecompileId::Bls12Pairing, PAIRING_ADDRESS, run_jovian_pairing);

fn run_jovian_pairing(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    if input.len() > JOVIAN_PAIRING_MAX_INPUT_SIZE {
        return Err(PrecompileError::Fatal(
            "Pairing input length too long for Base input size limitation after the Jovian Hardfork"
                .to_string(),
        ));
    }
    Ok(call_eth_precompile(precompile::bls12_381::pairing::pairing, input, gas_limit, reservoir))
}

#[cfg(test)]
mod tests {
    use revm::{precompile::PrecompileError, primitives::Bytes};
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::g1_msm_isthmus(ISTHMUS_G1_MSM, ISTHMUS_G1_MSM_MAX_INPUT_SIZE, 260_000)]
    #[case::g1_msm_jovian(JOVIAN_G1_MSM, JOVIAN_G1_MSM_MAX_INPUT_SIZE, u64::MAX)]
    #[case::g2_msm_isthmus(ISTHMUS_G2_MSM, ISTHMUS_G2_MSM_MAX_INPUT_SIZE, 260_000)]
    #[case::g2_msm_jovian(JOVIAN_G2_MSM, JOVIAN_G2_MSM_MAX_INPUT_SIZE, u64::MAX)]
    #[case::pairing_isthmus(ISTHMUS_PAIRING, ISTHMUS_PAIRING_MAX_INPUT_SIZE, 260_000)]
    #[case::pairing_jovian(JOVIAN_PAIRING, JOVIAN_PAIRING_MAX_INPUT_SIZE, u64::MAX)]
    fn test_max_size_rejects_oversized_input(
        #[case] precompile: Precompile,
        #[case] max_input_size: usize,
        #[case] gas_limit: u64,
    ) {
        let input = Bytes::from(vec![0u8; max_input_size + 1]);
        let result = precompile.execute(&input, gas_limit, 0);
        assert!(
            matches!(&result, Err(PrecompileError::Fatal(msg)) if msg.contains("input length too long")),
            "expected Fatal error for oversized input, got {result:?}"
        );
    }
}
