//! Compares the ZK programs this build proves with the ones the chain verifies.
//!
//! The verify step of this test checks the SNARK against a verifying key it
//! computes from its own ELFs, so on its own it passes however far the deployed
//! contracts have drifted. The chain checks the same proof against two
//! `immutable` hashes on the `AggregateVerifier` the factory points at:
//!
//! - `ZK_AGGREGATE_HASH`, passed to the SP1 verifier as the aggregation
//!   program's verifying key;
//! - `ZK_RANGE_HASH`, which the aggregation program commits as the range
//!   program's verifying key inside the journal.
//!
//! Either mismatch makes every on-chain ZK proof revert `InvalidProof()` — the
//! failure the challenger E2E hit on zeronet after zk-host moved to base v1.4.2
//! ahead of the on-chain hashes — while this test kept passing.
//!
//! Only meaningful when this binary and zk-host are built from the same base
//! commit: the computed keys come from this binary's ELFs, the proofs from
//! zk-host's.

use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, sol};
use anyhow::{Context, Result, bail};
use base_proof_zk_utils::types::u32_to_u8;
use sp1_sdk::{HashableKey, SP1VerifyingKey};

sol! {
    function gameImpls(uint32 gameType) external view returns (address);
    function ZK_AGGREGATE_HASH() external view returns (bytes32);
    function ZK_RANGE_HASH() external view returns (bytes32);
}

/// The two program hashes an `AggregateVerifier` checks ZK proofs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramHashes {
    /// Aggregation program verifying key, as `ZK_AGGREGATE_HASH` stores it.
    pub aggregate: B256,
    /// Range program verifying key, as `ZK_RANGE_HASH` stores it.
    pub range: B256,
}

impl ProgramHashes {
    /// Encodes locally computed verifying keys the way the contract stores them.
    ///
    /// Same encodings as `crates/proof/zk/backend/scripts/bin/vkeys.rs`, which
    /// produces the values that go into the deployment tasks.
    pub fn computed(range_vk: &SP1VerifyingKey, aggregation_vk: &SP1VerifyingKey) -> Result<Self> {
        let aggregate = aggregation_vk
            .bytes32()
            .parse::<B256>()
            .context("aggregation verifying key is not a 32-byte hex value")?;
        let range = B256::from(u32_to_u8(range_vk.hash_u32()));
        Ok(Self { aggregate, range })
    }

    /// Reads the hashes of the implementation `factory` currently registers for
    /// `game_type`, and returns that implementation's address with them.
    pub async fn onchain<P: Provider>(
        provider: &P,
        factory: Address,
        game_type: u32,
    ) -> Result<(Address, Self)> {
        let implementation = gameImplsCall::abi_decode_returns(
            &call(provider, factory, gameImplsCall { gameType: game_type }.abi_encode()).await?,
        )
        .context("failed to decode gameImpls")?;
        if implementation == Address::ZERO {
            bail!("factory {factory} has no implementation registered for game type {game_type}");
        }
        let aggregate = ZK_AGGREGATE_HASHCall::abi_decode_returns(
            &call(provider, implementation, ZK_AGGREGATE_HASHCall {}.abi_encode()).await?,
        )
        .context("failed to decode ZK_AGGREGATE_HASH")?;
        let range = ZK_RANGE_HASHCall::abi_decode_returns(
            &call(provider, implementation, ZK_RANGE_HASHCall {}.abi_encode()).await?,
        )
        .context("failed to decode ZK_RANGE_HASH")?;
        Ok((implementation, Self { aggregate, range }))
    }

    /// Fails when either hash differs, naming every hash that does and both
    /// values for each, so the error alone says what to rotate or roll back.
    pub fn ensure_match(onchain: Self, computed: Self, implementation: Address) -> Result<()> {
        let mut mismatches = Vec::new();
        if onchain.aggregate != computed.aggregate {
            mismatches.push(format!(
                "ZK_AGGREGATE_HASH is {} on-chain but this build's aggregation program is {}",
                onchain.aggregate, computed.aggregate
            ));
        }
        if onchain.range != computed.range {
            mismatches.push(format!(
                "ZK_RANGE_HASH is {} on-chain but this build's range program is {}",
                onchain.range, computed.range
            ));
        }
        if mismatches.is_empty() {
            return Ok(());
        }
        bail!(
            "the AggregateVerifier at {implementation} would reject proofs from this build with \
             InvalidProof(): {}. Either register an implementation with this build's hashes or \
             roll the prover back to the build they were computed from",
            mismatches.join("; ")
        )
    }
}

async fn call<P: Provider>(provider: &P, to: Address, data: Vec<u8>) -> Result<Bytes> {
    provider
        .call(TransactionRequest::default().to(to).input(Bytes::from(data).into()))
        .await
        .with_context(|| format!("eth_call to {to} failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMPLEMENTATION: Address = Address::repeat_byte(0x90);

    fn hashes(aggregate: u8, range: u8) -> ProgramHashes {
        ProgramHashes { aggregate: B256::repeat_byte(aggregate), range: B256::repeat_byte(range) }
    }

    #[test]
    fn matching_hashes_pass() {
        ProgramHashes::ensure_match(hashes(1, 2), hashes(1, 2), IMPLEMENTATION)
            .expect("identical hashes match");
    }

    #[test]
    fn a_range_mismatch_names_both_values_and_only_that_hash() {
        let error = ProgramHashes::ensure_match(hashes(1, 2), hashes(1, 3), IMPLEMENTATION)
            .expect_err("range differs")
            .to_string();
        assert!(error.contains("ZK_RANGE_HASH"), "{error}");
        assert!(error.contains(&B256::repeat_byte(2).to_string()), "{error}");
        assert!(error.contains(&B256::repeat_byte(3).to_string()), "{error}");
        assert!(!error.contains("ZK_AGGREGATE_HASH"), "{error}");
        assert!(error.contains(&IMPLEMENTATION.to_string()), "{error}");
    }

    #[test]
    fn both_mismatches_are_reported_together() {
        let error = ProgramHashes::ensure_match(hashes(1, 2), hashes(4, 5), IMPLEMENTATION)
            .expect_err("both differ")
            .to_string();
        assert!(error.contains("ZK_AGGREGATE_HASH"), "{error}");
        assert!(error.contains("ZK_RANGE_HASH"), "{error}");
    }

    /// The range hash is the verifying key's eight words, big-endian, which is
    /// how the aggregation program commits it (`u32_to_u8`) and how the
    /// deployment tasks record it.
    #[test]
    fn range_encoding_is_big_endian_words() {
        let words = [0x0011_2233, 0x4455_6677, 0, 0, 0, 0, 0, 0x8899_aabb];
        let encoded = B256::from(u32_to_u8(words));
        assert_eq!(&encoded[..8], &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]);
        assert_eq!(&encoded[28..], &[0x88, 0x99, 0xaa, 0xbb]);
    }
}
