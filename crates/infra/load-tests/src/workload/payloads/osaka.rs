use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes};
use alloy_rpc_types::TransactionRequest;

use super::Payload;
use crate::{config::OsakaTarget, workload::SeededRng};

/// CLZ opcode value (EIP-7939, Base Azul / Osaka).
const CLZ_OPCODE: u8 = 0x1e;

/// P256VERIFY precompile address 0x0000…0100 (EIP-7951, Osaka pricing 6 900 gas).
const P256VERIFY_OSAKA_ADDR: Address =
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]);

/// MODEXP precompile address (0x05).
const MODEXP_ADDR: Address = Address::with_last_byte(5);

/// Repetitions of `PUSH1 <byte> + CLZ + POP` in the CREATE initcode.
const CLZ_ITERATIONS: usize = 150;

/// Gas limit for CLZ CREATE transactions (~21k intrinsic + 32k CREATE + execution).
const CLZ_GAS_LIMIT: u64 = 80_000;

/// Gas limit for P256VERIFY Osaka calls (21k intrinsic + 6 900 precompile + margin).
const P256VERIFY_OSAKA_GAS_LIMIT: u64 = 30_000;

/// Gas limit for MODEXP Osaka calls (21k intrinsic + tripled cost + margin; min gas 500).
const MODEXP_OSAKA_GAS_LIMIT: u64 = 30_000;

/// Generates transactions that exercise Osaka (Base Azul) opcodes and precompiles:
///
/// - [`OsakaTarget::Clz`]: CREATE with initcode that loops the CLZ opcode (EIP-7939).
/// - [`OsakaTarget::P256verifyOsaka`]: call to precompile 0x0100 at Osaka pricing (EIP-7951).
/// - [`OsakaTarget::ModexpOsaka`]: MODEXP call under EIP-7823 + EIP-7883 rules.
#[derive(Debug, Clone)]
pub struct OsakaPayload {
    target: OsakaTarget,
}

impl OsakaPayload {
    /// Creates a new Osaka payload for the given target.
    pub const fn new(target: OsakaTarget) -> Self {
        Self { target }
    }

    /// CREATE transaction with initcode that exercises the CLZ opcode (EIP-7939).
    ///
    /// Initcode: `CLZ_ITERATIONS × (PUSH1 <rand> + CLZ + POP) + STOP`.
    /// Leaving `to` unset (None) produces a CREATE transaction.
    fn generate_clz(rng: &mut SeededRng) -> TransactionRequest {
        let mut initcode = Vec::with_capacity(CLZ_ITERATIONS * 4 + 1);
        for _ in 0..CLZ_ITERATIONS {
            initcode.push(0x60); // PUSH1
            initcode.push(rng.gen_range(0u8..=255u8));
            initcode.push(CLZ_OPCODE); // CLZ — counts leading zeros of top-of-stack word
            initcode.push(0x50); // POP
        }
        initcode.push(0x00); // STOP

        TransactionRequest::default()
            .with_input(Bytes::from(initcode))
            .with_gas_limit(CLZ_GAS_LIMIT)
        // `to` is None → CREATE transaction
    }

    /// Call to the P256VERIFY precompile (0x0100) with Osaka gas pricing (EIP-7951).
    ///
    /// Input: 160 bytes — `msg_hash(32)` + r(32) + s(32) + `pub_x(32)` + `pub_y(32)`.
    /// Random data will not produce a valid signature, but the precompile executes and
    /// charges the full 6 900 gas.
    fn generate_p256verify_osaka(rng: &mut SeededRng) -> TransactionRequest {
        let mut input = [0u8; 160];
        for byte in &mut input {
            *byte = rng.gen_range(0u8..=255u8);
        }

        TransactionRequest::default()
            .with_to(P256VERIFY_OSAKA_ADDR)
            .with_input(Bytes::from(input.to_vec()))
            .with_gas_limit(P256VERIFY_OSAKA_GAS_LIMIT)
    }

    /// MODEXP call compliant with Osaka rules (EIP-7823 + EIP-7883).
    ///
    /// Field lengths are kept ≤ 32 bytes (well within the 1 024-byte EIP-7823 cap).
    /// Gas limit is set above the new minimum of 500 (EIP-7883).
    fn generate_modexp_osaka(rng: &mut SeededRng) -> TransactionRequest {
        let base_len = rng.gen_range(1usize..=32usize);
        let exp_len = rng.gen_range(1usize..=32usize);
        let mod_len = rng.gen_range(1usize..=32usize);

        let mut data = vec![0u8; 96 + base_len + exp_len + mod_len];
        data[31] = base_len as u8;
        data[63] = exp_len as u8;
        data[95] = mod_len as u8;

        for byte in &mut data[96..96 + base_len] {
            *byte = rng.gen_range(0u8..=255u8);
        }
        for byte in &mut data[96 + base_len..96 + base_len + exp_len] {
            *byte = rng.gen_range(0u8..=255u8);
        }
        // Ensure modulus MSB is non-zero so the precompile has meaningful work.
        data[96 + base_len + exp_len] = rng.gen_range(1u8..=255u8);
        for byte in &mut data[96 + base_len + exp_len + 1..96 + base_len + exp_len + mod_len] {
            *byte = rng.gen_range(0u8..=255u8);
        }

        TransactionRequest::default()
            .with_to(MODEXP_ADDR)
            .with_input(Bytes::from(data))
            .with_gas_limit(MODEXP_OSAKA_GAS_LIMIT)
    }
}

impl Payload for OsakaPayload {
    fn name(&self) -> &'static str {
        "osaka"
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, _to: Address) -> TransactionRequest {
        match &self.target {
            OsakaTarget::Clz => Self::generate_clz(rng),
            OsakaTarget::P256verifyOsaka => Self::generate_p256verify_osaka(rng),
            OsakaTarget::ModexpOsaka => Self::generate_modexp_osaka(rng),
        }
    }
}
