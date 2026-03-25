use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes};
use alloy_rpc_types::TransactionRequest;
use revm::precompile::{PrecompileId, PrecompileSpecId};

use super::Payload;
use crate::workload::SeededRng;

/// Parses a precompile identifier from a config string.
pub fn parse_precompile_id(s: &str) -> Result<PrecompileId, String> {
    match s.to_lowercase().as_str() {
        "ecrecover" | "ec_recover" => Ok(PrecompileId::EcRec),
        "sha256" => Ok(PrecompileId::Sha256),
        "ripemd160" | "ripemd" => Ok(PrecompileId::Ripemd160),
        "identity" => Ok(PrecompileId::Identity),
        "modexp" => Ok(PrecompileId::ModExp),
        "bn254_add" | "bn128_add" | "ecadd" => Ok(PrecompileId::Bn254Add),
        "bn254_mul" | "bn128_mul" | "ecmul" => Ok(PrecompileId::Bn254Mul),
        "bn254_pairing" | "bn128_pairing" | "ecpairing" => Ok(PrecompileId::Bn254Pairing),
        "blake2f" | "blake2" => Ok(PrecompileId::Blake2F),
        "kzg_point_evaluation" | "kzg" => Ok(PrecompileId::KzgPointEvaluation),
        other => Err(format!("unknown precompile: {other}")),
    }
}

fn precompile_address(id: &PrecompileId) -> Address {
    id.precompile(PrecompileSpecId::CANCUN).map(|p| *p.address()).unwrap_or_else(|| {
        let byte = match id {
            PrecompileId::Sha256 => 0x02,
            PrecompileId::Ripemd160 => 0x03,
            PrecompileId::Identity => 0x04,
            PrecompileId::ModExp => 0x05,
            PrecompileId::Bn254Add => 0x06,
            PrecompileId::Bn254Mul => 0x07,
            PrecompileId::Bn254Pairing => 0x08,
            PrecompileId::Blake2F => 0x09,
            PrecompileId::KzgPointEvaluation => 0x0a,
            _ => 0x01,
        };
        Address::with_last_byte(byte)
    })
}

/// Generates transactions that call EVM precompiled contracts.
#[derive(Debug, Clone)]
pub struct PrecompilePayload {
    id: PrecompileId,
    blake2f_rounds: Option<u32>,
    iterations: u32,
    looper_contract: Option<Address>,
}

impl PrecompilePayload {
    /// Creates a new precompile payload.
    pub const fn new(id: PrecompileId) -> Self {
        Self { id, blake2f_rounds: None, iterations: 1, looper_contract: None }
    }

    /// Creates a new precompile payload with all options.
    pub const fn with_options(
        id: PrecompileId,
        blake2f_rounds: Option<u32>,
        iterations: u32,
        looper_contract: Option<Address>,
    ) -> Self {
        Self { id, blake2f_rounds, iterations, looper_contract }
    }

    fn encode_identity_data(rng: &mut SeededRng) -> Bytes {
        Bytes::from(rng.gen_bytes::<128>().to_vec())
    }

    fn encode_sha256_data(rng: &mut SeededRng) -> Bytes {
        Bytes::from(rng.gen_bytes::<64>().to_vec())
    }

    fn encode_ecrecover_data(rng: &mut SeededRng) -> Bytes {
        let mut data = vec![0u8; 128];
        data[0..32].copy_from_slice(&rng.gen_bytes::<32>());
        data[63] = 27 + (rng.gen_range(0..=1) as u8);
        data[64..96].copy_from_slice(&rng.gen_bytes::<32>());
        data[96..128].copy_from_slice(&rng.gen_bytes::<32>());
        Bytes::from(data)
    }

    fn encode_modexp_data(rng: &mut SeededRng) -> Bytes {
        let base_len = rng.gen_range(1..=32);
        let exp_len = rng.gen_range(1..=32);
        let mod_len = rng.gen_range(1..=32);

        let mut data = vec![0u8; 96 + base_len + exp_len + mod_len];

        data[31] = base_len as u8;
        data[63] = exp_len as u8;
        data[95] = mod_len as u8;

        for i in 0..base_len {
            data[96 + i] = rng.gen_range(0..=255);
        }
        for i in 0..exp_len {
            data[96 + base_len + i] = rng.gen_range(0..=255);
        }
        data[96 + base_len + exp_len] = rng.gen_range(1..=255);
        for i in 1..mod_len {
            data[96 + base_len + exp_len + i] = rng.gen_range(0..=255);
        }

        Bytes::from(data)
    }

    fn encode_bn254_add_data() -> Bytes {
        Bytes::from(vec![0u8; 128])
    }

    fn encode_bn254_mul_data() -> Bytes {
        Bytes::from(vec![0u8; 96])
    }

    const fn encode_bn254_pairing_data() -> Bytes {
        Bytes::new()
    }

    fn encode_blake2f_data(rng: &mut SeededRng, fixed_rounds: Option<u32>) -> (Bytes, u64) {
        let mut data = vec![0u8; 213];
        let rounds = fixed_rounds.unwrap_or_else(|| rng.gen_range(1..=400_000));
        data[0..4].copy_from_slice(&rounds.to_be_bytes());

        for byte in &mut data[4..212] {
            *byte = rng.gen_range(0..=255);
        }
        data[212] = 1;

        // blake2f costs exactly `rounds` gas. The 30k base covers the 21k intrinsic
        // gas plus calldata costs for the 213-byte input.
        let gas_limit = 30_000 + u64::from(rounds);
        (Bytes::from(data), gas_limit)
    }

    fn encode_kzg_data() -> Bytes {
        Bytes::from(vec![0u8; 192])
    }
}

impl PrecompilePayload {
    fn encode_looper_call(precompile: Address, data: &Bytes, iterations: u32) -> Bytes {
        // loopCall(address,bytes,uint256) selector: keccak256("loopCall(address,bytes,uint256)")[:4]
        // = 0x7b395fc2
        let selector = [0x7b, 0x39, 0x5f, 0xc2];

        // ABI encode: address (32 bytes) + offset to bytes (32) + iterations (32) + bytes length (32) + bytes data (padded)
        let data_len = data.len();
        let padded_data_len = data_len.div_ceil(32) * 32;

        let mut encoded = Vec::with_capacity(4 + 32 + 32 + 32 + 32 + padded_data_len);
        encoded.extend_from_slice(&selector);

        // address precompile (left-padded to 32 bytes)
        encoded.extend_from_slice(&[0u8; 12]);
        encoded.extend_from_slice(precompile.as_slice());

        // offset to bytes data (points to position 96 = 0x60)
        encoded.extend_from_slice(&[0u8; 31]);
        encoded.push(0x60);

        // uint256 iterations
        let mut iter_bytes = [0u8; 32];
        iter_bytes[28..32].copy_from_slice(&iterations.to_be_bytes());
        encoded.extend_from_slice(&iter_bytes);

        // bytes length
        let mut len_bytes = [0u8; 32];
        len_bytes[28..32].copy_from_slice(&(data_len as u32).to_be_bytes());
        encoded.extend_from_slice(&len_bytes);

        // bytes data (padded to 32 bytes)
        encoded.extend_from_slice(data);
        encoded.resize(encoded.len() + padded_data_len - data_len, 0);

        Bytes::from(encoded)
    }
}

impl Payload for PrecompilePayload {
    fn name(&self) -> &'static str {
        "precompile"
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, _to: Address) -> TransactionRequest {
        let (precompile_data, single_gas) = match self.id {
            PrecompileId::Identity => (Self::encode_identity_data(rng), 100_000u64),
            PrecompileId::Sha256 | PrecompileId::Ripemd160 => {
                (Self::encode_sha256_data(rng), 100_000)
            }
            PrecompileId::EcRec => (Self::encode_ecrecover_data(rng), 30_000),
            PrecompileId::ModExp => (Self::encode_modexp_data(rng), 250_000),
            PrecompileId::Bn254Add => (Self::encode_bn254_add_data(), 25_000),
            PrecompileId::Bn254Mul => (Self::encode_bn254_mul_data(), 30_000),
            PrecompileId::Bn254Pairing => (Self::encode_bn254_pairing_data(), 70_000),
            PrecompileId::Blake2F => Self::encode_blake2f_data(rng, self.blake2f_rounds),
            PrecompileId::KzgPointEvaluation => (Self::encode_kzg_data(), 75_000),
            _ => (Bytes::from(rng.gen_bytes::<32>().to_vec()), 100_000),
        };

        let precompile_addr = precompile_address(&self.id);

        if self.iterations > 1
            && let Some(looper) = self.looper_contract
        {
            let data = Self::encode_looper_call(precompile_addr, &precompile_data, self.iterations);
            // Gas: base cost + (per-iteration cost * iterations) + some overhead for looper
            let gas_limit = 50_000 + single_gas * u64::from(self.iterations);
            return TransactionRequest::default()
                .with_to(looper)
                .with_input(data)
                .with_gas_limit(gas_limit);
        }

        TransactionRequest::default()
            .with_to(precompile_addr)
            .with_input(precompile_data)
            .with_gas_limit(single_gas)
    }
}
