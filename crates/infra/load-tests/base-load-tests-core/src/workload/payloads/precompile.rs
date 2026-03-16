use alloy_primitives::{Address, Bytes};
use revm::precompile::{PrecompileId, PrecompileSpecId};

use super::Payload;
use crate::{rpc::TransactionRequest, workload::SeededRng};

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
    *id.precompile(PrecompileSpecId::CANCUN)
        .expect("standard precompiles must have addresses")
        .address()
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
}

impl Payload for PrecompilePayload {
    fn name(&self) -> &'static str {
        "precompile"
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, _to: Address) -> TransactionRequest {
        let data = match self.id {
            PrecompileId::Identity => Self::encode_identity_data(rng),
            PrecompileId::Sha256 | PrecompileId::Ripemd160 => Self::encode_sha256_data(rng),
            _ => Bytes::from(rng.gen_bytes::<32>().to_vec()),
        };

        TransactionRequest::contract_call(precompile_address(&self.id), data)
            .with_gas_limit(100_000)
    }
}
