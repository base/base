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
}

impl PrecompilePayload {
    /// Creates a new precompile payload.
    pub const fn new(id: PrecompileId) -> Self {
        Self { id }
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

    fn encode_blake2f_data(rng: &mut SeededRng) -> (Bytes, u64) {
        let mut data = vec![0u8; 213];
        let rounds = rng.gen_range(1..=400_000) as u32;
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

impl Payload for PrecompilePayload {
    fn name(&self) -> &'static str {
        "precompile"
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, _to: Address) -> TransactionRequest {
        let (data, gas_limit) = match self.id {
            PrecompileId::Identity => (Self::encode_identity_data(rng), 100_000),
            PrecompileId::Sha256 | PrecompileId::Ripemd160 => {
                (Self::encode_sha256_data(rng), 100_000)
            }
            PrecompileId::EcRec => (Self::encode_ecrecover_data(rng), 30_000),
            PrecompileId::ModExp => (Self::encode_modexp_data(rng), 250_000),
            PrecompileId::Bn254Add => (Self::encode_bn254_add_data(), 25_000),
            PrecompileId::Bn254Mul => (Self::encode_bn254_mul_data(), 30_000),
            PrecompileId::Bn254Pairing => (Self::encode_bn254_pairing_data(), 70_000),
            PrecompileId::Blake2F => Self::encode_blake2f_data(rng),
            PrecompileId::KzgPointEvaluation => (Self::encode_kzg_data(), 75_000),
            _ => (Bytes::from(rng.gen_bytes::<32>().to_vec()), 100_000),
        };

        TransactionRequest::default()
            .with_to(precompile_address(&self.id))
            .with_input(data)
            .with_gas_limit(gas_limit)
    }
}
