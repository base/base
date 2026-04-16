use revm::precompile::{Precompile, PrecompileError, PrecompileId, PrecompileResult, bn254};

/// Max input size for the bn254 pair precompile after the Granite hardfork.
pub const GRANITE_MAX_INPUT_SIZE: usize = 112687;
/// Bn254 pair precompile with Granite input limits.
pub const GRANITE: Precompile =
    Precompile::new(PrecompileId::Bn254Pairing, bn254::pair::ADDRESS, run_pair_granite);

/// Run the bn254 pair precompile with Granite input limit.
pub fn run_pair_granite(input: &[u8], gas_limit: u64) -> PrecompileResult {
    if input.len() > GRANITE_MAX_INPUT_SIZE {
        return Err(PrecompileError::Bn254PairLength);
    }
    bn254::run_pair(
        input,
        bn254::pair::ISTANBUL_PAIR_PER_POINT,
        bn254::pair::ISTANBUL_PAIR_BASE,
        gas_limit,
    )
}

/// Max input size for the bn254 pair precompile after the Jovian hardfork.
pub const JOVIAN_MAX_INPUT_SIZE: usize = 81_984;
/// Bn254 pair precompile with Jovian input limits.
pub const JOVIAN: Precompile =
    Precompile::new(PrecompileId::Bn254Pairing, bn254::pair::ADDRESS, run_pair_jovian);

/// Run the bn254 pair precompile with Jovian input limit.
pub fn run_pair_jovian(input: &[u8], gas_limit: u64) -> PrecompileResult {
    if input.len() > JOVIAN_MAX_INPUT_SIZE {
        return Err(PrecompileError::Bn254PairLength);
    }
    bn254::run_pair(
        input,
        bn254::pair::ISTANBUL_PAIR_PER_POINT,
        bn254::pair::ISTANBUL_PAIR_BASE,
        gas_limit,
    )
}

#[cfg(test)]
mod tests {
    use revm::{
        precompile::{PrecompileError, bn254},
        primitives::hex,
    };

    use super::*;

    #[test]
    fn test_bn254_pair_granite() {
        let input = hex::decode(
            "\
      1c76476f4def4bb94541d57ebba1193381ffa7aa76ada664dd31c16024c43f59\
      3034dd2920f673e204fee2811c678745fc819b55d3e9d294e45c9b03a76aef41\
      209dd15ebff5d46c4bd888e51a93cf99a7329636c63514396b4a452003a35bf7\
      04bf11ca01483bfa8b34b43561848d28905960114c8ac04049af4b6315a41678\
      2bb8324af6cfc93537a2ad1a445cfd0ca2a71acd7ac41fadbf933c2a51be344d\
      120a2a4cf30c1bf9845f20c6fe39e07ea2cce61f0c9bb048165fe5e4de877550\
      111e129f1cf1097710d41c4ac70fcdfa5ba2023c6ff1cbeac322de49d1b6df7c\
      2032c61a830e3c17286de9462bf242fca2883585b93870a73853face6a6bf411\
      198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2\
      1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed\
      090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b\
      12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa",
        )
        .unwrap();
        let expected =
            hex::decode("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let outcome = run_pair_granite(&input, 260_000).unwrap();
        assert_eq!(outcome.bytes, expected);

        // Invalid input length
        let bad_input = hex::decode(
            "\
          1111111111111111111111111111111111111111111111111111111111111111\
          1111111111111111111111111111111111111111111111111111111111111111\
          111111111111111111111111111111\
      ",
        )
        .unwrap();
        assert!(matches!(
            run_pair_granite(&bad_input, 260_000),
            Err(PrecompileError::Bn254PairLength)
        ));

        // Valid input length shorter than 112687
        let at_gas_limit = vec![1u8; 586 * bn254::PAIR_ELEMENT_LEN];
        assert!(matches!(run_pair_granite(&at_gas_limit, 260_000), Err(PrecompileError::OutOfGas)));

        // Input length longer than 112687
        let over_limit = vec![1u8; 587 * bn254::PAIR_ELEMENT_LEN];
        assert!(matches!(
            run_pair_granite(&over_limit, 260_000),
            Err(PrecompileError::Bn254PairLength)
        ));
    }

    #[test]
    fn test_bn254_pair_jovian() {
        const TEST_INPUT: [u8; 384] = hex!(
            "2cf44499d5d27bb186308b7af7af02ac5bc9eeb6a3d147c186b21fb1b76e18da2c0f001f52110ccfe69108924926e45f0b0c868df0e7bde1fe16d3242dc715f61fb19bb476f6b9e44e2a32234da8212f61cd63919354bc06aef31e3cfaff3ebc22606845ff186793914e03e21df544c34ffe2f2f3504de8a79d9159eca2d98d92bd368e28381e8eccb5fa81fc26cf3f048eea9abfdd85d7ed3ab3698d63e4f902fe02e47887507adf0ff1743cbac6ba291e66f59be6bd763950bb16041a0a85e000000000000000000000000000000000000000000000000000000000000000130644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd451971ff0471b09fa93caaf13cbf443c1aede09cc4328f5a62aad45f40ec133eb4091058a3141822985733cbdddfed0fd8d6c104e9e9eff40bf5abfef9ab163bc72a23af9a5ce2ba2796c1f4e453a370eb0af8c212d9dc9acd8fc02c2e907baea223a8eb0b0996252cb548a4487da97b02422ebc0e834613f954de6c7e0afdc1fc"
        );
        const EXPECTED_OUTPUT: [u8; 32] =
            hex!("0000000000000000000000000000000000000000000000000000000000000001");

        let res = run_pair_jovian(TEST_INPUT.as_ref(), u64::MAX);
        assert!(matches!(res, Ok(outcome) if **outcome.bytes == EXPECTED_OUTPUT));
    }

    #[test]
    fn test_bn254_pair_jovian_bad_input_len() {
        let input = [0u8; JOVIAN_MAX_INPUT_SIZE + 1];
        assert!(matches!(run_pair_jovian(&input, u64::MAX), Err(PrecompileError::Bn254PairLength)));
    }
}
