//! Deterministic in-process build/validate differential fuzz test.
//!
//! Builds blocks from fuzzed transaction sets on a single in-process node and
//! checks that the executor accepts what the payload builder produced.
//! `build_block_from_transactions` runs `getPayload` (the builder path) and then
//! `newPayload` (the validator path); an error there means the builder produced a
//! block the validator rejects, the class of disagreement that halts the chain.
//!
//! Beyond build-vs-validate, each block is checked against single-implementation
//! invariants (gas within limit), and a second test asserts determinism: the same
//! seed builds byte-identical blocks (matching state roots and hashes) twice.
//!
//! Runs fully in-process (no devnet, no Docker), forces exactly the generated
//! transactions into each block (`no_tx_pool`), and uses a fixed per-block
//! timestamp, so a run is deterministic and a failing seed replays exactly.
//! Override the seed with `FUZZ_SEED`.

use alloy_consensus::SignableTransaction;
use alloy_eips::{
    BlockNumberOrTag,
    eip2718::Encodable2718,
    eip2930::{AccessList, AccessListItem},
};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use base_common_rpc_types::BaseTransactionRequest;
use base_node_runner::test_utils::TestHarness;
use base_test_utils::{Account, DEVNET_CHAIN_ID};
use eyre::{Result, eyre};
use rand::{Rng, SeedableRng, rngs::StdRng};

/// Default seed so a run replays deterministically. Override with `FUZZ_SEED`.
const DEFAULT_SEED: u64 = 0x_D1FF_0000;
/// Blocks to build in one run.
const NUM_BLOCKS: u64 = 20;
/// Transactions per block.
const TXS_PER_BLOCK: usize = 12;
/// Generous per-tx gas so generated transactions are always includable.
const TX_GAS_LIMIT: u64 = 200_000;

/// Pre-funded test accounts used as senders (all allocated in the test genesis).
const SENDERS: [Account; 3] = [Account::Alice, Account::Bob, Account::Charlie];

#[tokio::test]
async fn fuzz_build_validate() -> Result<()> {
    let seed = seed_from_env();
    // Building the sequence exercises build-vs-validate (an error is a divergence)
    // and the per-block invariants; the observations are discarded here.
    run_sequence(seed).await?;
    Ok(())
}

/// Determinism oracle: the same seed must build byte-identical blocks (matching
/// state roots and hashes) across independent runs. A mismatch means block
/// production is non-deterministic, which would make any divergence unreplayable.
#[tokio::test]
async fn fuzz_build_validate_deterministic() -> Result<()> {
    let seed = seed_from_env();
    let first = run_sequence(seed).await?;
    let second = run_sequence(seed).await?;

    if first.len() != second.len() {
        return Err(eyre!(
            "nondeterministic block count (seed={seed:#x}): {} != {}",
            first.len(),
            second.len()
        ));
    }
    for (a, b) in first.iter().zip(second.iter()) {
        if a != b {
            return Err(eyre!(
                "nondeterministic build at block {} (seed={seed:#x}): {a:?} != {b:?}",
                a.number
            ));
        }
    }
    Ok(())
}

/// Prove-red: the validator path (`newPayload`) actually rejects a bad block, so a
/// green build/validate run is meaningful rather than a rubber stamp. Builds a valid
/// payload, confirms it validates, then validates a tampered version (a wrong
/// `parent_beacon_block_root`, which makes the recomputed block hash mismatch the
/// payload's claimed hash) and requires an INVALID result.
#[tokio::test]
async fn oracle_rejects_tampered_block() -> Result<()> {
    let harness = TestHarness::new().await?;
    let mut generator = FuzzTxGenerator::new(seed_from_env());
    let mut nonces = [0u64; SENDERS.len()];

    let mut txs = Vec::new();
    for _ in 0..5 {
        let shape = generator.next_shape();
        txs.push(sign_tx(&SENDERS[shape.sender], &shape, nonces[shape.sender])?);
        nonces[shape.sender] += 1;
    }

    let built = harness.build_payload(txs).await?;

    // Sanity: the untampered payload is accepted.
    let clean = harness.validate_payload(&built).await?;
    assert!(!clean.status.is_invalid(), "untampered payload should validate, got {clean:?}");

    // Inject a divergence: a wrong parent_beacon_block_root makes the recomputed block
    // hash mismatch the payload's claimed hash, so the validator must reject it.
    let bad_pbbr = if built.parent_beacon_block_root == B256::repeat_byte(0xAB) {
        B256::ZERO
    } else {
        B256::repeat_byte(0xAB)
    };
    let tampered = harness
        .engine()
        .new_payload(
            built.execution_payload.clone(),
            vec![],
            bad_pbbr,
            built.execution_requests.clone(),
        )
        .await?;
    assert!(
        tampered.status.is_invalid(),
        "validator accepted a tampered block (parent_beacon_block_root mismatch): {tampered:?}"
    );

    Ok(())
}

/// Observations recorded for each built block, used for invariants and the
/// determinism comparison.
#[derive(Debug, PartialEq, Eq)]
struct BlockObs {
    number: u64,
    hash: B256,
    state_root: B256,
    gas_used: u64,
    gas_limit: u64,
}

/// Build `NUM_BLOCKS` blocks of fuzzed transactions on a fresh in-process node,
/// asserting build-vs-validate agreement and per-block invariants, and return the
/// per-block observations.
async fn run_sequence(seed: u64) -> Result<Vec<BlockObs>> {
    let harness = TestHarness::new().await?;
    let mut generator = FuzzTxGenerator::new(seed);
    let mut nonces = [0u64; SENDERS.len()];
    let mut observations = Vec::with_capacity(NUM_BLOCKS as usize);

    for block in 0..NUM_BLOCKS {
        let mut txs = Vec::with_capacity(TXS_PER_BLOCK);
        for _ in 0..TXS_PER_BLOCK {
            let shape = generator.next_shape();
            let raw = sign_tx(&SENDERS[shape.sender], &shape, nonces[shape.sender])?;
            nonces[shape.sender] += 1;
            txs.push(raw);
        }

        // Build (getPayload) then validate (newPayload). An error is a build-vs-validate
        // divergence: the builder produced a block the validator rejects.
        harness.build_block_from_transactions(txs).await.map_err(|e| {
            eyre!("build/validate divergence at block {block} (seed={seed:#x}): {e}")
        })?;

        let header = harness
            .provider()
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre!("no block found after building block {block} (seed={seed:#x})"))?
            .header;

        // Invariant: a block can never use more gas than its own limit.
        if header.gas_used > header.gas_limit {
            return Err(eyre!(
                "invariant violated at block {block} (seed={seed:#x}): \
                 gas_used {} > gas_limit {}",
                header.gas_used,
                header.gas_limit
            ));
        }

        observations.push(BlockObs {
            number: header.number,
            hash: header.hash,
            state_root: header.state_root,
            gas_used: header.gas_used,
            gas_limit: header.gas_limit,
        });
    }

    Ok(observations)
}

fn seed_from_env() -> u64 {
    match std::env::var("FUZZ_SEED") {
        Ok(s) if !s.is_empty() => s
            .strip_prefix("0x")
            .map_or_else(|| s.parse(), |h| u64::from_str_radix(h, 16))
            .unwrap_or_else(|_| panic!("FUZZ_SEED={s:?} is not a valid u64")),
        _ => DEFAULT_SEED,
    }
}

/// A generated transaction shape, before nonce assignment and signing.
struct TxShape {
    sender: usize,
    to: Address,
    value: U256,
    input: Bytes,
    access_list: AccessList,
}

/// Seeded generator for the fuzzed transaction stream. Kept simple for a first cut:
/// valid transfers and small random calldata across the funded senders. The
/// adversarial permutation layer (access lists, gas edges, tx-type mix) slots in here.
struct FuzzTxGenerator {
    rng: StdRng,
}

impl FuzzTxGenerator {
    fn new(seed: u64) -> Self {
        Self { rng: StdRng::seed_from_u64(seed) }
    }

    fn next_shape(&mut self) -> TxShape {
        let sender = self.rng.random_range(0..SENDERS.len());
        let to = SENDERS[self.rng.random_range(0..SENDERS.len())].address();
        let value = U256::from(self.rng.random_range(1..=1_000_000u64));
        // ~1 in 5 transactions carry small random calldata.
        let input = if self.rng.random_range(0..5) == 0 {
            let len = self.rng.random_range(0..64usize);
            let mut bytes = vec![0u8; len];
            self.rng.fill(&mut bytes[..]);
            Bytes::from(bytes)
        } else {
            Bytes::new()
        };
        TxShape { sender, to, value, input, access_list: self.next_access_list() }
    }

    /// Generate an EIP-2930 access list over a small shared pool of addresses and
    /// storage slots. Because the pool is small and only ~40% of txs carry a list,
    /// the same slots recur across txs (declared/warm in some, absent/cold in
    /// others), exercising the access-list gas accounting and warm/cold set setup
    /// that builder and validator must agree on.
    fn next_access_list(&mut self) -> AccessList {
        if self.rng.random_range(0..5) >= 2 {
            return AccessList::default();
        }
        let items = (0..self.rng.random_range(1..=2))
            .map(|_| {
                let address = SENDERS[self.rng.random_range(0..SENDERS.len())].address();
                let storage_keys = (0..self.rng.random_range(0..=3))
                    .map(|_| B256::from(U256::from(self.rng.random_range(0..8u64))))
                    .collect();
                AccessListItem { address, storage_keys }
            })
            .collect();
        AccessList(items)
    }
}

/// Complete a shape into a signed, EIP-2718 encoded transaction on the test chain.
fn sign_tx(account: &Account, shape: &TxShape, nonce: u64) -> Result<Bytes> {
    let signer = account.signer();
    let request = BaseTransactionRequest::default()
        .from(signer.address())
        .to(shape.to)
        .value(shape.value)
        .with_input(shape.input.clone())
        .transaction_type(2)
        .with_gas_limit(TX_GAS_LIMIT)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(DEVNET_CHAIN_ID)
        .with_access_list(shape.access_list.clone())
        .with_nonce(nonce);
    let typed =
        request.build_typed_tx().map_err(|req| eyre!("invalid transaction request: {req:?}"))?;
    let signature = signer.sign_hash_sync(&typed.signature_hash())?;
    Ok(typed.into_signed(signature).encoded_2718().into())
}
