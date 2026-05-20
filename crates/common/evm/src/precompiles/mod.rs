//! Base precompile provider integration.

use crate::BaseSpecId;

/// Base precompile provider for the Base EVM spec.
pub type BasePrecompiles = base_common_precompiles::BasePrecompiles<BaseSpecId>;

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use revm::{
        precompile::{PrecompileError, bn254, modexp, secp256r1},
        primitives::eip7823,
    };

    use super::*;
    use crate::BaseUpgrade;

    fn encode_length(len: usize) -> [u8; 32] {
        let mut encoded = [0u8; 32];
        encoded[24..].copy_from_slice(&(len as u64).to_be_bytes());
        encoded
    }

    fn oversized_modexp_input() -> Vec<u8> {
        let mut input = Vec::with_capacity(96);
        input.extend_from_slice(&encode_length(eip7823::INPUT_SIZE_LIMIT + 1));
        input.extend_from_slice(&encode_length(0));
        input.extend_from_slice(&encode_length(1));
        input
    }

    #[test]
    fn base_spec_id_selects_jovian_precompile_limits() {
        let precompiles = BasePrecompiles::new_with_spec(BaseSpecId::new(BaseUpgrade::Jovian));
        let bn254_pair = precompiles.precompiles().get(&bn254::pair::ADDRESS).unwrap();

        let input = vec![0u8; 81_984 + bn254::PAIR_ELEMENT_LEN];
        assert!(matches!(
            bn254_pair.execute(&input, u64::MAX),
            Err(PrecompileError::Bn254PairLength)
        ));
    }

    #[test]
    fn base_spec_id_selects_azul_osaka_precompile_rules() {
        let jovian_precompiles =
            BasePrecompiles::new_with_spec(BaseSpecId::new(BaseUpgrade::Jovian));
        let azul_precompiles = BasePrecompiles::new_with_spec(BaseSpecId::new(BaseUpgrade::Azul));

        let jovian_p256 =
            jovian_precompiles.precompiles().get(secp256r1::P256VERIFY.address()).unwrap();
        let azul_p256 =
            azul_precompiles.precompiles().get(secp256r1::P256VERIFY_OSAKA.address()).unwrap();

        assert!(jovian_p256.execute(&[], 5_000).is_ok());
        assert!(matches!(azul_p256.execute(&[], 5_000), Err(PrecompileError::OutOfGas)));

        let azul_modexp = azul_precompiles.precompiles().get(modexp::OSAKA.address()).unwrap();
        assert!(matches!(
            azul_modexp.execute(&oversized_modexp_input(), u64::MAX),
            Err(PrecompileError::ModexpEip7823LimitSize)
        ));
    }
}
