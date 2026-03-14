use alloy_primitives::{Address, Bytes};

use super::Payload;
use crate::{rpc::TransactionRequest, workload::SeededRng};

/// Generates transactions that call EVM precompiled contracts.
#[derive(Debug, Clone)]
pub struct PrecompilePayload {
    /// Which precompile to call.
    pub target: PrecompileTarget,
}

/// Available precompile targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrecompileTarget {
    /// ECRECOVER (0x01) - signature recovery.
    EcRecover,
    /// SHA256 (0x02) - SHA-256 hash.
    Sha256,
    /// RIPEMD160 (0x03) - RIPEMD-160 hash.
    Ripemd160,
    /// IDENTITY (0x04) - data copy.
    Identity,
    /// MODEXP (0x05) - modular exponentiation.
    ModExp,
    /// `EC_ADD` (0x06) - elliptic curve addition.
    EcAdd,
    /// `EC_MUL` (0x07) - elliptic curve multiplication.
    EcMul,
    /// `EC_PAIRING` (0x08) - elliptic curve pairing.
    EcPairing,
    /// BLAKE2F (0x09) - BLAKE2 compression.
    Blake2f,
}

impl PrecompileTarget {
    const fn address(&self) -> Address {
        match self {
            Self::EcRecover => {
                Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
            }
            Self::Sha256 => {
                Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2])
            }
            Self::Ripemd160 => {
                Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3])
            }
            Self::Identity => {
                Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4])
            }
            Self::ModExp => {
                Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5])
            }
            Self::EcAdd => {
                Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6])
            }
            Self::EcMul => {
                Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7])
            }
            Self::EcPairing => {
                Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8])
            }
            Self::Blake2f => {
                Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9])
            }
        }
    }
}

impl PrecompilePayload {
    /// Creates a new precompile payload.
    pub const fn new(target: PrecompileTarget) -> Self {
        Self { target }
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
        let data = match self.target {
            PrecompileTarget::Identity => Self::encode_identity_data(rng),
            PrecompileTarget::Sha256 | PrecompileTarget::Ripemd160 => Self::encode_sha256_data(rng),
            _ => Bytes::from(rng.gen_bytes::<32>().to_vec()),
        };

        TransactionRequest::contract_call(self.target.address(), data).with_gas_limit(100_000)
    }
}
