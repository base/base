//! This module contains the L1 block fee calculation function.
use alloy_primitives::{B64, U256};

use crate::utils::flz_compress_len;
use core::ops::Mul;

/// Cost per byte in calldata
const ZERO_BYTE_COST: u64 = 4;
const NON_ZERO_BYTE_COST: u64 = 16;

/// Calculate the data gas for posting the transaction on L1.
///
/// In bedrock, calldata costs 16 gas per non-zero byte and 4 gas per zero byte, with
/// an extra 68 non-zero bytes were included in the rollup data costs to account for the empty
/// signature.
pub fn data_gas_bedrock(input: &[u8]) -> U256 {
    data_gas_regolith(input) + U256::from(NON_ZERO_BYTE_COST).mul(U256::from(68))
}

/// Calculate the data gas for posting the transaction on L1.
///
/// In regolith, calldata costs 16 gas per non-zero byte and 4 gas per zero byte
pub fn data_gas_regolith(input: &[u8]) -> U256 {
    let rollup_data_gas_cost = U256::from(input.iter().fold(0, |acc, byte| {
        acc + if *byte == 0x00 { ZERO_BYTE_COST } else { NON_ZERO_BYTE_COST }
    }));

    rollup_data_gas_cost
}

/// Calculate the data gas for posting the transaction on L1.
///
/// In fjord, Calldata costs 16 gas per byte after compression.
pub fn data_gas_fjord(input: &[u8]) -> U256 {
    let estimated_size = tx_estimated_size_fjord(input);
    estimated_size
        .saturating_mul(U256::from(NON_ZERO_BYTE_COST))
        .wrapping_div(U256::from(1_000_000))
}

// Calculate the estimated compressed transaction size in bytes, scaled by 1e6.
// This value is computed based on the following formula:
// max(minTransactionSize, intercept + fastlzCoef*fastlzSize)
fn tx_estimated_size_fjord(input: &[u8]) -> U256 {
    let fastlz_size = U256::from(flz_compress_len(input));

    fastlz_size
        .saturating_mul(U256::from(836_500))
        .saturating_sub(U256::from(42_585_600))
        .max(U256::from(100_000_000))
}

/// Calculate the gas cost of a transaction based on L1 block data posted on L2 post-bedrock.
pub fn calculate_tx_l1_cost_bedrock(
    input: &[u8],
    l1_fee_overhead: U256,
    base_fee: U256,
    l1_fee_scalar: U256,
) -> U256 {
    if input.is_empty() || input.first() == Some(&0x7F) {
        return U256::ZERO;
    }

    let rollup_data_gas_cost = data_gas_bedrock(input);
    rollup_data_gas_cost
        .saturating_add(l1_fee_overhead)
        .saturating_mul(base_fee)
        .saturating_mul(l1_fee_scalar)
        .wrapping_div(U256::from(1_000_000))
}

/// Calculate the gas cost of a transaction based on L1 block data posted on L2 post-regolith.
pub fn calculate_tx_l1_cost_regolith(
    input: &[u8],
    l1_fee_overhead: U256,
    base_fee: U256,
    l1_fee_scalar: U256,
) -> U256 {
    if input.is_empty() || input.first() == Some(&0x7F) {
        return U256::ZERO;
    }

    let rollup_data_gas_cost = data_gas_regolith(input);
    rollup_data_gas_cost
        .saturating_add(l1_fee_overhead)
        .saturating_mul(base_fee)
        .saturating_mul(l1_fee_scalar)
        .wrapping_div(U256::from(1_000_000))
}

/// Calculate the gas cost of a transaction based on L1 block data posted on L2, post-Ecotone.
///
/// L1 cost function:
/// `(calldataGas/16)*(l1BaseFee*16*l1BaseFeeScalar + l1BlobBaseFee*l1BlobBaseFeeScalar)/1e6`
///
/// We divide "calldataGas" by 16 to change from units of calldata gas to "estimated # of bytes when
/// compressed". Known as "compressedTxSize" in the spec.
///
/// Function is actually computed as follows for better precision under integer arithmetic:
/// `calldataGas*(l1BaseFee*16*l1BaseFeeScalar + l1BlobBaseFee*l1BlobBaseFeeScalar)/16e6`
pub fn calculate_tx_l1_cost_ecotone(
    input: &[u8],
    base_fee: U256,
    base_fee_scalar: U256,
    blob_base_fee: U256,
    blob_base_fee_scalar: U256,
) -> U256 {
    if input.is_empty() || input.first() == Some(&0x7F) {
        return U256::ZERO;
    }

    let rollup_data_gas_cost = data_gas_regolith(input);
    let l1_fee_scaled = calculate_l1_fee_scaled_ecotone(
        base_fee,
        base_fee_scalar,
        blob_base_fee,
        blob_base_fee_scalar,
    );

    l1_fee_scaled
        .saturating_mul(rollup_data_gas_cost)
        .wrapping_div(U256::from(1_000_000 * NON_ZERO_BYTE_COST))
}

/// Calculate the gas cost of a transaction based on L1 block data posted on L2, post-Fjord.
///
/// L1 cost function:
/// `estimatedSize*(baseFeeScalar*l1BaseFee*16 + blobFeeScalar*l1BlobBaseFee)/1e12`
pub fn calculate_tx_l1_cost_fjord(
    input: &[u8],
    base_fee: U256,
    base_fee_scalar: U256,
    blob_base_fee: U256,
    blob_base_fee_scalar: U256,
) -> U256 {
    if input.is_empty() || input.first() == Some(&0x7F) {
        return U256::ZERO;
    }

    let l1_fee_scaled = calculate_l1_fee_scaled_ecotone(
        base_fee,
        base_fee_scalar,
        blob_base_fee,
        blob_base_fee_scalar,
    );
    let estimated_size = tx_estimated_size_fjord(input);

    estimated_size.saturating_mul(l1_fee_scaled).wrapping_div(U256::from(1_000_000_000_000u64))
}

// l1BaseFee*16*l1BaseFeeScalar + l1BlobBaseFee*l1BlobBaseFeeScalar
fn calculate_l1_fee_scaled_ecotone(
    base_fee: U256,
    base_fee_scalar: U256,
    blob_base_fee: U256,
    blob_base_fee_scalar: U256,
) -> U256 {
    let calldata_cost_per_byte: U256 =
        base_fee.saturating_mul(U256::from(NON_ZERO_BYTE_COST)).saturating_mul(base_fee_scalar);
    let blob_cost_per_byte = blob_base_fee.saturating_mul(blob_base_fee_scalar);

    U256::from(calldata_cost_per_byte).saturating_add(blob_cost_per_byte)
}

/// Extracts the Holocene 1599 parameters from the encoded form:
/// <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/holocene/exec-engine.md#eip1559params-encoding>
///
/// Returns (`elasticity`, `denominator`)
pub fn decode_eip_1559_params(eip_1559_params: B64) -> (u32, u32) {
    let denominator: [u8; 4] = eip_1559_params.0[..4].try_into().expect("sufficient length");
    let elasticity: [u8; 4] = eip_1559_params.0[4..8].try_into().expect("sufficient length");

    (u32::from_be_bytes(elasticity), u32::from_be_bytes(denominator))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{bytes, hex};

    #[test]
    fn test_data_gas_bedrock() {
        // 0xFACADE = 6 nibbles = 3 bytes
        // 0xFACADE = 1111 1010 . 1100 1010 . 1101 1110
        let input_1 = bytes!("FACADE");

        // 0xFA00CA00DE = 10 nibbles = 5 bytes
        // 0xFA00CA00DE = 1111 1010 . 0000 0000 . 1100 1010 . 0000 0000 . 1101 1110
        let input_2 = bytes!("FA00CA00DE");

        // Pre-regolith (ie bedrock) has an extra 68 non-zero bytes
        // gas cost = 3 non-zero bytes * NON_ZERO_BYTE_COST + NON_ZERO_BYTE_COST * 68
        // gas cost = 3 * 16 + 68 * 16 = 1136
        let bedrock_data_gas = data_gas_bedrock(&input_1);
        assert_eq!(bedrock_data_gas, U256::from(1136));

        // Pre-regolith (ie bedrock) has an extra 68 non-zero bytes
        // gas cost = 3 non-zero * NON_ZERO_BYTE_COST + 2 * ZERO_BYTE_COST + NON_ZERO_BYTE_COST * 68
        // gas cost = 3 * 16 + 2 * 4 + 68 * 16 = 1144
        let bedrock_data_gas = data_gas_bedrock(&input_2);
        assert_eq!(bedrock_data_gas, U256::from(1144));
    }

    #[test]
    fn test_data_gas_regolith() {
        // 0xFACADE = 6 nibbles = 3 bytes
        // 0xFACADE = 1111 1010 . 1100 1010 . 1101 1110
        let input_1 = bytes!("FACADE");

        // 0xFA00CA00DE = 10 nibbles = 5 bytes
        // 0xFA00CA00DE = 1111 1010 . 0000 0000 . 1100 1010 . 0000 0000 . 1101 1110
        let input_2 = bytes!("FA00CA00DE");

        // gas cost = 3 non-zero bytes * NON_ZERO_BYTE_COST
        // gas cost = 3 * 16 = 48
        let bedrock_data_gas = data_gas_regolith(&input_1);
        assert_eq!(bedrock_data_gas, U256::from(48));

        // gas cost = 3 non-zero * NON_ZERO_BYTE_COST + 2 * ZERO_BYTE_COST
        // gas cost = 3 * 16 + 2 * 4 = 56
        let bedrock_data_gas = data_gas_regolith(&input_2);
        assert_eq!(bedrock_data_gas, U256::from(56));
    }

    #[test]
    fn test_data_gas_fjord() {
        // 0xFACADE = 6 nibbles = 3 bytes
        // 0xFACADE = 1111 1010 . 1100 1010 . 1101 1110
        let input_1 = bytes!("FACADE");

        // 0xFA00CA00DE = 10 nibbles = 5 bytes
        // 0xFA00CA00DE = 1111 1010 . 0000 0000 . 1100 1010 . 0000 0000 . 1101 1110
        let input_2 = bytes!("FA00CA00DE");

        // Fjord has a minimum compressed size of 100 bytes
        // gas cost = 100 * 16 = 1600
        let fjord_data_gas = data_gas_fjord(&input_1);
        assert_eq!(fjord_data_gas, U256::from(1600));

        // Fjord has a minimum compressed size of 100 bytes
        // gas cost = 100 * 16 = 1600
        let fjord_data_gas = data_gas_fjord(&input_2);
        assert_eq!(fjord_data_gas, U256::from(1600));
    }

    #[test]
    fn test_calculate_tx_l1_cost_bedrock() {
        // let mut l1_block_bedrock = get_default_bedrock_l1_info();
        let base_fee = U256::from(1_000);
        let l1_fee_overhead = U256::from(1_000);
        let l1_fee_scalar = U256::from(1_000);

        // calldataGas * (l1BaseFee * 16 * l1BaseFeeScalar + l1BlobBaseFee * l1BlobBaseFeeScalar) /
        // (16 * 1e6) = (16 * 3 + 16 * 68 + 1000) * 1000 * 1000 / (1_000_000)
        // = 2136
        let input = bytes!("FACADE");
        let gas_cost =
            calculate_tx_l1_cost_bedrock(&input, l1_fee_overhead, base_fee, l1_fee_scalar);
        assert_eq!(gas_cost, U256::from(2136));

        // Zero rollup data gas cost should result in zero
        let input = bytes!("");
        let gas_cost =
            calculate_tx_l1_cost_bedrock(&input, l1_fee_overhead, base_fee, l1_fee_scalar);
        assert_eq!(gas_cost, U256::ZERO);

        // Deposit transactions with the EIP-2718 type of 0x7F should result in zero
        let input = bytes!("7FFACADE");
        let gas_cost =
            calculate_tx_l1_cost_bedrock(&input, l1_fee_overhead, base_fee, l1_fee_scalar);
        assert_eq!(gas_cost, U256::ZERO);
    }

    #[test]
    fn test_calculate_tx_l1_cost_regolith() {
        // let mut l1_block_bedrock = get_default_bedrock_l1_info();
        let base_fee = U256::from(1_000);
        let l1_fee_overhead = U256::from(1_000);
        let l1_fee_scalar = U256::from(1_000);

        // calldataGas * (l1BaseFee * 16 * l1BaseFeeScalar + l1BlobBaseFee * l1BlobBaseFeeScalar) /
        // (16 * 1e6) = (16 * 3 + 1000) * 1000 * 1000 / (1_000_000)
        // = 1048
        let input = bytes!("FACADE");
        let gas_cost =
            calculate_tx_l1_cost_regolith(&input, l1_fee_overhead, base_fee, l1_fee_scalar);
        assert_eq!(gas_cost, U256::from(1048));

        // Zero rollup data gas cost should result in zero
        let input = bytes!("");
        let gas_cost =
            calculate_tx_l1_cost_regolith(&input, l1_fee_overhead, base_fee, l1_fee_scalar);
        assert_eq!(gas_cost, U256::ZERO);

        // Deposit transactions with the EIP-2718 type of 0x7F should result in zero
        let input = bytes!("7FFACADE");
        let gas_cost =
            calculate_tx_l1_cost_regolith(&input, l1_fee_overhead, base_fee, l1_fee_scalar);
        assert_eq!(gas_cost, U256::ZERO);
    }

    #[test]
    fn test_calculate_tx_l1_cost_ecotone() {
        let base_fee = U256::from(1_000);
        let blob_base_fee = U256::from(1_000);
        let blob_base_fee_scalar = U256::from(1_000);
        let base_fee_scalar = U256::from(1_000);

        // calldataGas * (l1BaseFee * 16 * l1BaseFeeScalar + l1BlobBaseFee * l1BlobBaseFeeScalar) /
        // (16 * 1e6) = (16 * 3) * (1000 * 16 * 1000 + 1000 * 1000) / (16 * 1e6)
        // = 51
        let input = bytes!("FACADE");
        let gas_cost = calculate_tx_l1_cost_ecotone(
            &input,
            base_fee,
            base_fee_scalar,
            blob_base_fee,
            blob_base_fee_scalar,
        );
        assert_eq!(gas_cost, U256::from(51));

        // Zero rollup data gas cost should result in zero
        let input = bytes!("");
        let gas_cost = calculate_tx_l1_cost_ecotone(
            &input,
            base_fee,
            base_fee_scalar,
            blob_base_fee,
            blob_base_fee_scalar,
        );
        assert_eq!(gas_cost, U256::ZERO);

        // Deposit transactions with the EIP-2718 type of 0x7F should result in zero
        let input = bytes!("7FFACADE");
        let gas_cost = calculate_tx_l1_cost_ecotone(
            &input,
            base_fee,
            base_fee_scalar,
            blob_base_fee,
            blob_base_fee_scalar,
        );
        assert_eq!(gas_cost, U256::ZERO);
    }

    #[test]
    fn test_calculate_tx_l1_cost_fjord() {
        // l1FeeScaled = baseFeeScalar*l1BaseFee*16 + blobFeeScalar*l1BlobBaseFee
        //             = 1000 * 1000 * 16 + 1000 * 1000
        //             = 17e6
        let base_fee = U256::from(1_000);
        let blob_base_fee = U256::from(1_000);
        let blob_base_fee_scalar = U256::from(1_000);
        let base_fee_scalar = U256::from(1_000);

        // fastLzSize = 4
        // estimatedSize = max(minTransactionSize, intercept + fastlzCoef*fastlzSize)
        //               = max(100e6, 836500*4 - 42585600)
        //               = 100e6
        let input = bytes!("FACADE");
        // l1Cost = estimatedSize * l1FeeScaled / 1e12
        //        = 100e6 * 17 / 1e6
        //        = 1700
        let gas_cost = calculate_tx_l1_cost_fjord(
            &input,
            base_fee,
            base_fee_scalar,
            blob_base_fee,
            blob_base_fee_scalar,
        );
        assert_eq!(gas_cost, U256::from(1700));

        // fastLzSize = 202
        // estimatedSize = max(minTransactionSize, intercept + fastlzCoef*fastlzSize)
        //               = max(100e6, 836500*202 - 42585600)
        //               = 126387400
        let input = bytes!("02f901550a758302df1483be21b88304743f94f80e51afb613d764fa61751affd3313c190a86bb870151bd62fd12adb8e41ef24f3f000000000000000000000000000000000000000000000000000000000000006e000000000000000000000000af88d065e77c8cc2239327c5edb3a432268e5831000000000000000000000000000000000000000000000000000000000003c1e5000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000148c89ed219d02f1a5be012c689b4f5b731827bebe000000000000000000000000c001a033fd89cb37c31b2cba46b6466e040c61fc9b2a3675a7f5f493ebd5ad77c497f8a07cdf65680e238392693019b4092f610222e71b7cec06449cb922b93b6a12744e");
        // l1Cost = estimatedSize * l1FeeScaled / 1e12
        //        = 126387400 * 17 / 1e6
        //        = 2148
        let gas_cost = calculate_tx_l1_cost_fjord(
            &input,
            base_fee,
            base_fee_scalar,
            blob_base_fee,
            blob_base_fee_scalar,
        );
        assert_eq!(gas_cost, U256::from(2148));

        // Zero rollup data gas cost should result in zero
        let input = bytes!("");
        let gas_cost = calculate_tx_l1_cost_fjord(
            &input,
            base_fee,
            base_fee_scalar,
            blob_base_fee,
            blob_base_fee_scalar,
        );
        assert_eq!(gas_cost, U256::ZERO);

        // Deposit transactions with the EIP-2718 type of 0x7F should result in zero
        let input = bytes!("7FFACADE");
        let gas_cost = calculate_tx_l1_cost_fjord(
            &input,
            base_fee,
            base_fee_scalar,
            blob_base_fee,
            blob_base_fee_scalar,
        );
        assert_eq!(gas_cost, U256::ZERO);
    }

    #[test]
    fn calculate_tx_l1_cost_fjord_actual_block() {
        // L1 block info for OP mainnet fjord block 124665056
        // <https://optimistic.etherscan.io/block/124665056>
        let base_fee = U256::from(1055991687);
        let blob_base_fee = U256::from(1);
        let blob_base_fee_scalar = U256::from(1014213);
        let base_fee_scalar = U256::from(5227);

        // second tx in OP mainnet Fjord block 124665056
        // <https://optimistic.etherscan.io/tx/0x1059e8004daff32caa1f1b1ef97fe3a07a8cf40508f5b835b66d9420d87c4a4a>
        const TX: &[u8] = &hex!("02f904940a8303fba78401d6d2798401db2b6d830493e0943e6f4f7866654c18f536170780344aa8772950b680b904246a761202000000000000000000000000087000a300de7200382b55d40045000000e5d60e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003a0000000000000000000000000000000000000000000000000000000000000022482ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000120000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000044095ea7b300000000000000000000000021c4928109acb0659a88ae5329b5374a3024694c0000000000000000000000000000000000000000000000049b9ca9a6943400000000000000000000000000000000000000000000000000000000000000000000000000000000000021c4928109acb0659a88ae5329b5374a3024694c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000024b6b55f250000000000000000000000000000000000000000000000049b9ca9a694340000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000415ec214a3950bea839a7e6fbb0ba1540ac2076acd50820e2d5ef83d0902cdffb24a47aff7de5190290769c4f0a9c6fabf63012986a0d590b1b571547a8c7050ea1b00000000000000000000000000000000000000000000000000000000000000c080a06db770e6e25a617fe9652f0958bd9bd6e49281a53036906386ed39ec48eadf63a07f47cf51a4a40b4494cf26efc686709a9b03939e20ee27e59682f5faa536667e");

        // l1 gas used for tx and l1 fee for tx, from OP mainnet block scanner
        // https://optimistic.etherscan.io/tx/0x1059e8004daff32caa1f1b1ef97fe3a07a8cf40508f5b835b66d9420d87c4a4a
        let expected_data_gas = U256::from(4471);
        let expected_l1_fee = U256::from_be_bytes(hex!(
            "00000000000000000000000000000000000000000000000000000005bf1ab43d"
        ));

        // test
        let data_gas = data_gas_fjord(TX);
        assert_eq!(data_gas, expected_data_gas);
        let l1_fee = calculate_tx_l1_cost_fjord(
            TX,
            base_fee,
            base_fee_scalar,
            blob_base_fee,
            blob_base_fee_scalar,
        );
        assert_eq!(l1_fee, expected_l1_fee)
    }
}
