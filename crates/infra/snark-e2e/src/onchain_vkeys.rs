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

use alloy_primitives::{Address, B256};
use alloy_provider::RootProvider;
use anyhow::{Context, Result, bail};
use base_proof_contracts::{
    AggregateVerifierContractClient, DisputeGameFactoryClient, DisputeGameFactoryContractClient,
};
use base_proof_zk_utils::types::u32_to_u8;
use sp1_sdk::{HashableKey, SP1VerifyingKey};

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
    pub async fn onchain(
        provider: &RootProvider,
        factory: Address,
        game_type: u32,
    ) -> Result<(Address, Self)> {
        let implementation = DisputeGameFactoryContractClient::new(factory, provider.clone())
            .game_impls(game_type)
            .await?;
        if implementation == Address::ZERO {
            bail!("factory {factory} has no implementation registered for game type {game_type}");
        }
        let verifier = AggregateVerifierContractClient::new(provider.clone());
        let aggregate = verifier.zk_aggregate_hash(implementation).await?;
        let range = verifier.zk_range_hash(implementation).await?;
        Ok((implementation, Self { aggregate, range }))
    }

    /// Fails when either hash differs, naming every hash that does and both
    /// values for each, so the error alone says what to rotate or roll back.
    pub fn ensure_match(self, computed: Self, implementation: Address) -> Result<()> {
        let mut mismatches = Vec::new();
        if self.aggregate != computed.aggregate {
            mismatches.push(format!(
                "ZK_AGGREGATE_HASH is {} on-chain but this build's aggregation program is {}",
                self.aggregate, computed.aggregate
            ));
        }
        if self.range != computed.range {
            mismatches.push(format!(
                "ZK_RANGE_HASH is {} on-chain but this build's range program is {}",
                self.range, computed.range
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

#[cfg(test)]
mod tests {
    use super::*;

    const IMPLEMENTATION: Address = Address::repeat_byte(0x90);

    fn hashes(aggregate: u8, range: u8) -> ProgramHashes {
        ProgramHashes { aggregate: B256::repeat_byte(aggregate), range: B256::repeat_byte(range) }
    }

    #[test]
    fn matching_hashes_pass() {
        hashes(1, 2).ensure_match(hashes(1, 2), IMPLEMENTATION).expect("identical hashes match");
    }

    #[test]
    fn a_range_mismatch_names_both_values_and_only_that_hash() {
        let error = hashes(1, 2)
            .ensure_match(hashes(1, 3), IMPLEMENTATION)
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
        let error = hashes(1, 2)
            .ensure_match(hashes(4, 5), IMPLEMENTATION)
            .expect_err("both differ")
            .to_string();
        assert!(error.contains("ZK_AGGREGATE_HASH"), "{error}");
        assert!(error.contains("ZK_RANGE_HASH"), "{error}");
    }
}
