//! Deterministic gas schedule for native BLS transactions.

use base_common_consensus::BlsPayload;

/// Deterministic gas schedule for native BLS transactions.
///
/// A native BLS transaction is charged exactly [`Self::intrinsic_gas`]: the amount depends only on
/// the payload, never on state, so the transaction pool, `eth_estimateGas` and execution agree.
/// There are no refunds and the outer gas limit must cover the full amount.
///
/// Batches charge exactly 21,000 execution gas per transfer, including batch overhead and input
/// processing. L1 data fees remain separate. This experimental price does not measure verification
/// or proving cost. Registration retains EIP-2537/2565 verification and EIP-2929/2200 storage prices.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlsGas;

impl BlsGas {
    /// Base transaction cost, as for any transaction.
    pub const TX_BASE: u64 = 21_000;
    /// Calldata cost per zero byte of the payload (EIP-2028).
    pub const INPUT_ZERO_BYTE: u64 = 4;
    /// Calldata cost per non-zero byte of the payload (EIP-2028).
    pub const INPUT_NONZERO_BYTE: u64 = 16;

    /// EIP-2537 pairing check base cost.
    pub const PAIRING_BASE: u64 = 37_700;
    /// EIP-2537 pairing check cost per G1/G2 pair.
    pub const PAIRING_PER_PAIR: u64 = 32_600;
    /// Hashing a message to G1: two EIP-2537 `MAP_FP_TO_G1` (5,500 each), one `G1ADD` (375) and
    /// an allowance for `expand_message_xmd` (SHA-256) and the two field reductions.
    pub const HASH_TO_G1: u64 = 12_000;
    /// Decompressing and validating a G1 point: the EIP-2565 price of one 381-bit modular
    /// exponentiation for the square root (~4,600) plus the subgroup check, priced as an
    /// EIP-2537 single-point `G1MSM` (12,000).
    pub const G1_POINT: u64 = 17_000;
    /// Decompressing and validating a G2 point: an Fp2 square root (~10,000, two Fp
    /// exponentiations) plus the subgroup check, priced as an EIP-2537 single-point `G2MSM`
    /// (22,500).
    pub const G2_POINT: u64 = 32_500;

    /// Reading or writing one 32-byte registry word cold (EIP-2929 `SLOAD`).
    pub const COLD_SLOAD: u64 = 2_100;
    /// Writing one fresh registry word: EIP-2200 zero-to-non-zero set plus the cold surcharge.
    pub const REGISTRY_WORD_WRITE: u64 = 20_000 + Self::COLD_SLOAD;
    /// Words of storage holding one registered compressed public key.
    pub const REGISTRY_KEY_WORDS: u64 = 3;
    /// Cold account access (EIP-2929).
    pub const COLD_ACCOUNT_ACCESS: u64 = 2_600;

    /// Registration: validate the key and the proof of possession, hash the key, run a two-pair
    /// pairing, write the three key words and touch the registry account.
    pub const REGISTRATION: u64 = Self::G2_POINT
        + Self::G1_POINT
        + Self::HASH_TO_G1
        + Self::PAIRING_BASE
        + 2 * Self::PAIRING_PER_PAIR
        + Self::REGISTRY_KEY_WORDS * Self::REGISTRY_WORD_WRITE
        + Self::COLD_ACCOUNT_ACCESS;

    /// All-in execution gas per transfer; no additional batch or input-byte charge.
    pub const BATCH_PER_TRANSFER: u64 = 21_000;

    /// Returns the exact gas a native BLS transaction with the given raw `input` and its decoded
    /// `payload` consumes.
    pub fn intrinsic_gas(input: &[u8], payload: &BlsPayload) -> u64 {
        match payload {
            BlsPayload::Registration { .. } => {
                let zero_bytes = input.iter().filter(|byte| **byte == 0).count() as u64;
                let input_gas = zero_bytes * Self::INPUT_ZERO_BYTE
                    + (input.len() as u64 - zero_bytes) * Self::INPUT_NONZERO_BYTE;
                Self::TX_BASE + input_gas + Self::REGISTRATION
            }
            BlsPayload::Batch { transfers, .. } => {
                transfers.len() as u64 * Self::BATCH_PER_TRANSFER
            }
        }
    }
}
