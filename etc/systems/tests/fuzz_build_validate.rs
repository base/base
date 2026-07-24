//! Deterministic in-process build/validate differential fuzz test.
//!
//! Builds blocks from fuzzed transaction sets on a single in-process node and
//! checks that the executor accepts what the payload builder produced.
//! `build_block_from_transactions` runs `getPayload` (the builder path) and then
//! `newPayload` (the validator path); an error there means the builder produced a
//! block the validator rejects, the class of disagreement that halts the chain.
//!
//! A contract is deployed into an early block so later transactions can call it
//! and touch storage; transactions carry fuzzed EIP-2930 access lists that warm
//! (or leave cold) those slots, exercising the warm/cold gas accounting builder
//! and validator must agree on. Each block is also checked against single-
//! implementation invariants (gas within limit), and a second test asserts
//! determinism: the same seed builds byte-identical blocks twice.
//!
//! Alongside build-vs-validate, `fuzz_derive_roundtrip` covers the other chain-
//! halting agreement boundary: batch-encode vs. derive. It runs fuzzed
//! transactions through Base's own span-batch codec (encode as the batcher does,
//! read back as the derivation pipeline does) and requires the reconstructed
//! blocks to match byte-for-byte.
//!
//! Runs fully in-process (no devnet, no Docker), forces exactly the generated
//! transactions into each block (`no_tx_pool`), and uses a fixed per-block
//! timestamp, so a run is deterministic and a failing seed replays exactly.
//! Override the seed with `FUZZ_SEED`.

use std::sync::Arc;

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
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use base_common_consensus::{Call, Eip8130Constants, Eip8130Signed, TxEip8130};
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, TestHarness};
use base_protocol::{RawSpanBatch, SingleBatch, SpanBatch};
use base_test_utils::{AccessListContract, Account, DEVNET_CHAIN_ID, build_test_genesis_cobalt};
use eyre::{Result, eyre};
use rand::{Rng, SeedableRng, rngs::StdRng};

/// Default seed so a run replays deterministically. Override with `FUZZ_SEED`.
const DEFAULT_SEED: u64 = 0x_D1FF_0000;
/// Blocks of fuzzed transactions to build in one run (after the deploy block).
const NUM_BLOCKS: u64 = 20;
/// Transactions per block.
const TXS_PER_BLOCK: usize = 12;
/// Generous per-tx gas so generated transactions are always includable.
const TX_GAS_LIMIT: u64 = 500_000;
/// Gas for the contract-deployment transaction.
const DEPLOY_GAS_LIMIT: u64 = 3_000_000;

/// Pre-funded test accounts used as senders (all allocated in the test genesis).
const SENDERS: [Account; 3] = [Account::Alice, Account::Bob, Account::Charlie];

/// Regression corpus: seeds that must always build cleanly. When a fuzz run finds a
/// build/validate divergence, add its seed here so the case replays deterministically
/// on every run forever. Seeded here with a spread of known-good starting points.
const CORPUS: &[u64] = &[DEFAULT_SEED, 0x1, 0xC0FFEE, 0xDEAD_BEEF];

/// Blocks of fuzzed EIP-8130 transactions to build in one run.
const NUM_8130_BLOCKS: u64 = 6;
/// EIP-8130 transactions per block.
const TXS_PER_BLOCK_8130: usize = 5;
/// Per-tx gas for generated EIP-8130 transactions (EOA-only call phases are cheap).
const TX_GAS_LIMIT_8130: u64 = 500_000;
/// A far-future expiry (well past any block timestamp here) used to fuzz the
/// `expiry` field without ever letting a transaction lapse and get dropped.
const FAR_FUTURE_EXPIRY: u64 = 4_000_000_000;

/// Blocks per fuzzed span batch in the derive round-trip.
const DERIVE_BLOCKS: usize = 8;
/// User transactions per block in the derive round-trip.
const DERIVE_TXS_PER_BLOCK: usize = 6;
/// L2 block time used to encode/derive the span batch (must match on both sides).
const DERIVE_BLOCK_TIME: u64 = 2;
/// Genesis timestamp the span batch's relative timestamps are computed against.
const DERIVE_GENESIS_TS: u64 = 0;
/// A single fixed L1 origin (epoch) for the whole span, so the derived epoch
/// numbers reconstruct deterministically from the origin bits.
const DERIVE_EPOCH: u64 = 1;

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

/// Regression corpus: every committed seed must build cleanly. This is the
/// deterministic replay gate — a divergence found by the nightly sweep is pinned
/// here (by seed) and re-checked on every run, so a fixed bug never regresses.
#[tokio::test]
async fn fuzz_build_validate_corpus() -> Result<()> {
    for &seed in CORPUS {
        run_sequence(seed)
            .await
            .map_err(|e| eyre!("regression corpus seed {seed:#x} failed: {e}"))?;
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

    // No contract deployed here; calls target an EOA (valid no-op) since this test
    // only needs a validly-built block to then tamper with.
    let contract = SENDERS[0].address();
    let mut txs = Vec::new();
    for _ in 0..5 {
        let shape = generator.next_shape(contract);
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

/// Deploy the access-list contract in a seed block, then build `NUM_BLOCKS` blocks
/// of fuzzed transactions on a fresh in-process node, asserting build-vs-validate
/// agreement and per-block invariants, and return the per-block observations.
async fn run_sequence(seed: u64) -> Result<Vec<BlockObs>> {
    let harness = TestHarness::new().await?;
    let mut generator = FuzzTxGenerator::new(seed);
    let mut nonces = [0u64; SENDERS.len()];

    // Seed pre-state: deploy the contract from the first sender so later blocks can
    // call it and touch storage. The CREATE address is deterministic (sender + nonce).
    let deployer = &SENDERS[0];
    let deploy = deploy_tx(deployer, nonces[0])?;
    harness
        .build_block_from_transactions(vec![deploy])
        .await
        .map_err(|e| eyre!("deploy block failed (seed={seed:#x}): {e}"))?;
    let contract = deployer.address().create(nonces[0]);
    nonces[0] += 1;

    let mut observations = Vec::with_capacity(NUM_BLOCKS as usize);
    for block in 0..NUM_BLOCKS {
        let mut txs = Vec::with_capacity(TXS_PER_BLOCK);
        for _ in 0..TXS_PER_BLOCK {
            let shape = generator.next_shape(contract);
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

/// Seeded generator for the fuzzed transaction stream: transfers, random calldata,
/// and contract calls that touch storage, each with a fuzzed access list. The
/// adversarial permutation layer (gas edges, more tx types) slots in here.
struct FuzzTxGenerator {
    rng: StdRng,
}

impl FuzzTxGenerator {
    fn new(seed: u64) -> Self {
        Self { rng: StdRng::seed_from_u64(seed) }
    }

    fn next_shape(&mut self, contract: Address) -> TxShape {
        let sender = self.rng.random_range(0..SENDERS.len());
        let access_list = self.next_access_list(contract);
        let (to, value, input) = match self.rng.random_range(0..5) {
            // ~40%: call the contract, touching a storage slot (SSTORE).
            0 | 1 => (contract, U256::ZERO, self.contract_call()),
            // ~20%: random calldata to an account.
            2 => {
                let len = self.rng.random_range(0..64usize);
                let mut bytes = vec![0u8; len];
                self.rng.fill(&mut bytes[..]);
                (self.random_recipient(), U256::ZERO, Bytes::from(bytes))
            }
            // ~40%: plain transfer.
            _ => (
                self.random_recipient(),
                U256::from(self.rng.random_range(1..=1_000_000u64)),
                Bytes::new(),
            ),
        };
        TxShape { sender, to, value, input, access_list }
    }

    fn random_recipient(&mut self) -> Address {
        SENDERS[self.rng.random_range(0..SENDERS.len())].address()
    }

    /// Encode a call that writes contract storage, so access-list warming has real
    /// SSTOREs to account for. Alternates between a fixed slot (`updateValue` ->
    /// slot 0) and mapping slots (`insertMultiple` -> keccak-derived slots), which
    /// broadens the warm/cold storage surface.
    fn contract_call(&mut self) -> Bytes {
        if self.rng.random_range(0..2) == 0 {
            let new_value = U256::from(self.rng.random_range(0..1_000u64));
            AccessListContract::updateValueCall { newValue: new_value }.abi_encode().into()
        } else {
            let n = self.rng.random_range(1..=4usize);
            let keys = (0..n).map(|_| U256::from(self.rng.random_range(0..16u64))).collect();
            let values = (0..n).map(|_| U256::from(self.rng.random_range(0..1_000u64))).collect();
            AccessListContract::insertMultipleCall { keys, values }.abi_encode().into()
        }
    }

    /// Generate an EIP-2930 access list that, ~40% of the time, declares the
    /// contract's low storage slots (warming them). Because only some txs declare
    /// them, the same slots are warm in some blocks and cold in others, exercising
    /// the warm/cold gas accounting builder and validator must agree on.
    fn next_access_list(&mut self, contract: Address) -> AccessList {
        if self.rng.random_range(0..5) >= 2 {
            return AccessList::default();
        }
        let storage_keys =
            (0..self.rng.random_range(0..=2)).map(|i| B256::from(U256::from(i as u64))).collect();
        AccessList(vec![AccessListItem { address: contract, storage_keys }])
    }
}

/// Build a signed contract-deployment transaction for the access-list contract.
fn deploy_tx(account: &Account, nonce: u64) -> Result<Bytes> {
    let signer = account.signer();
    let request = BaseTransactionRequest::default()
        .from(signer.address())
        .with_deploy_code(AccessListContract::BYTECODE.clone())
        .transaction_type(2)
        .with_gas_limit(DEPLOY_GAS_LIMIT)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(DEVNET_CHAIN_ID)
        .with_nonce(nonce);
    finish(&signer, request)
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
    finish(&signer, request)
}

/// Build, sign, and EIP-2718 encode a transaction request.
fn finish(signer: &PrivateKeySigner, request: BaseTransactionRequest) -> Result<Bytes> {
    let typed =
        request.build_typed_tx().map_err(|req| eyre!("invalid transaction request: {req:?}"))?;
    let signature = signer.sign_hash_sync(&typed.signature_hash())?;
    Ok(typed.into_signed(signature).encoded_2718().into())
}

/// Build-vs-validate over fuzzed **EIP-8130** transactions, Base's account-
/// abstraction transaction type (type `0x79`), which only activates on a
/// Cobalt-enabled chain. Each block forces a fuzzed set of 8130 transactions
/// (self-pay and sponsored, with a varying number of value-less call phases,
/// fuzzed expiry and metadata) through `getPayload`/`newPayload` and asserts the
/// builder and validator agree, that every forced transaction is actually mined,
/// and the per-block gas invariant holds.
#[tokio::test]
async fn fuzz_build_validate_eip8130() -> Result<()> {
    let seed = seed_from_env();
    run_eip8130_sequence(seed).await?;
    Ok(())
}

/// Determinism oracle for the EIP-8130 stream: the same seed must build byte-
/// identical blocks across independent runs, so a failing 8130 seed replays.
#[tokio::test]
async fn fuzz_build_validate_eip8130_deterministic() -> Result<()> {
    let seed = seed_from_env();
    let first = run_eip8130_sequence(seed).await?;
    let second = run_eip8130_sequence(seed).await?;

    if first.len() != second.len() {
        return Err(eyre!(
            "nondeterministic 8130 block count (seed={seed:#x}): {} != {}",
            first.len(),
            second.len()
        ));
    }
    for (a, b) in first.iter().zip(second.iter()) {
        if a != b {
            return Err(eyre!(
                "nondeterministic 8130 build at block {} (seed={seed:#x}): {a:?} != {b:?}",
                a.number
            ));
        }
    }
    Ok(())
}

/// Build `NUM_8130_BLOCKS` blocks of fuzzed EIP-8130 transactions on a fresh
/// Cobalt-enabled in-process node, asserting build-vs-validate agreement,
/// full inclusion, and the per-block gas invariant, and return the observations.
async fn run_eip8130_sequence(seed: u64) -> Result<Vec<BlockObs>> {
    // EIP-8130 requires the Cobalt fork; the default genesis is pre-Cobalt, so use
    // the Cobalt genesis (which funds the same accounts).
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_cobalt()));
    let harness = TestHarness::builder().with_chain_spec(chain_spec).build().await?;
    let mut generator = Eip8130Generator::new(seed);
    // Per-sender nonce-channel sequence (nonce_key = 0), advanced on inclusion.
    let mut sequences = [0u64; SENDERS.len()];

    let mut observations = Vec::with_capacity(NUM_8130_BLOCKS as usize);
    for block in 0..NUM_8130_BLOCKS {
        // The L1 block-info deposit must lead every block; the fuzzed 8130 txs follow.
        let mut txs = vec![L1_BLOCK_INFO_DEPOSIT_TX];
        for _ in 0..TXS_PER_BLOCK_8130 {
            let shape = generator.next_shape();
            let raw = sign_8130(&shape, sequences[shape.sender])?;
            sequences[shape.sender] += 1;
            txs.push(raw);
        }
        let expected = txs.len();

        // Build (getPayload) then validate (newPayload). An error is a build-vs-validate
        // divergence: the builder produced a block the validator rejects.
        harness.build_block_from_transactions(txs).await.map_err(|e| {
            eyre!("8130 build/validate divergence at block {block} (seed={seed:#x}): {e}")
        })?;

        let mined =
            harness.provider().get_block_by_number(BlockNumberOrTag::Latest).await?.ok_or_else(
                || eyre!("no block found after building 8130 block {block} (seed={seed:#x})"),
            )?;

        // Inclusion oracle: every forced transaction must be mined. A silently
        // dropped 8130 (the builder skipping it pre-execution) shows up here as a
        // short block, even though `build_block_from_transactions` returned `Ok`.
        let included = mined.transactions.len();
        if included != expected {
            return Err(eyre!(
                "8130 transaction dropped at block {block} (seed={seed:#x}): \
                 mined {included} of {expected} forced transactions"
            ));
        }

        // Invariant: a block can never use more gas than its own limit.
        let header = mined.header;
        if header.gas_used > header.gas_limit {
            return Err(eyre!(
                "invariant violated at 8130 block {block} (seed={seed:#x}): \
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

/// A generated EIP-8130 transaction shape, before nonce assignment and signing.
struct Eip8130Shape {
    /// Index into [`SENDERS`] of the transaction sender (recovered from `sender_auth`).
    sender: usize,
    /// Sponsoring payer index into [`SENDERS`], or `None` for a self-pay transaction.
    payer: Option<usize>,
    /// Call phases; each inner vector is a phase of value-less calls to EOA recipients.
    phases: Vec<Vec<Address>>,
    /// Transaction expiry (`0` disables it; otherwise a far-future timestamp).
    expiry: u64,
    /// Fuzzed metadata bytes (sometimes empty).
    metadata: Bytes,
}

/// Seeded generator for a fuzzed EIP-8130 transaction stream: self-pay and
/// sponsored transactions with a varying number of value-less call phases and
/// fuzzed expiry/metadata. All calls target funded EOAs so every phase succeeds
/// and the transaction is always includable, keeping the build-vs-validate and
/// inclusion oracles meaningful rather than tripping on generation noise.
struct Eip8130Generator {
    rng: StdRng,
}

impl Eip8130Generator {
    fn new(seed: u64) -> Self {
        Self { rng: StdRng::seed_from_u64(seed) }
    }

    fn next_shape(&mut self) -> Eip8130Shape {
        let sender = self.rng.random_range(0..SENDERS.len());
        // ~25% sponsored: a different funded account authorizes and pays the gas.
        let payer = if self.rng.random_range(0..4) == 0 {
            let offset = self.rng.random_range(1..SENDERS.len());
            Some((sender + offset) % SENDERS.len())
        } else {
            None
        };
        // 0..=2 phases, each with 1..=2 value-less calls to EOAs (always succeed).
        let num_phases = self.rng.random_range(0..=2);
        let phases = (0..num_phases)
            .map(|_| {
                let calls = self.rng.random_range(1..=2);
                (0..calls).map(|_| self.random_recipient()).collect()
            })
            .collect();
        let expiry = if self.rng.random_range(0..2) == 0 { 0 } else { FAR_FUTURE_EXPIRY };
        let metadata = if self.rng.random_range(0..2) == 0 {
            Bytes::new()
        } else {
            let len = self.rng.random_range(1..=4usize);
            let mut bytes = vec![0u8; len];
            self.rng.fill(&mut bytes[..]);
            Bytes::from(bytes)
        };
        Eip8130Shape { sender, payer, phases, expiry, metadata }
    }

    fn random_recipient(&mut self) -> Address {
        SENDERS[self.rng.random_range(0..SENDERS.len())].address()
    }
}

/// Complete an [`Eip8130Shape`] into a signed, EIP-2718 encoded type-`0x79`
/// transaction. The sender is recovered from `sender_auth` (so `sender` is left
/// `None`); a sponsored transaction additionally carries a K1 payer authenticator
/// over the payer digest (which binds to the resolved sender).
fn sign_8130(shape: &Eip8130Shape, sequence: u64) -> Result<Bytes> {
    let calls: Vec<Vec<Call>> = shape
        .phases
        .iter()
        .map(|phase| phase.iter().map(|to| Call { to: *to, data: Bytes::new() }).collect())
        .collect();
    let tx = TxEip8130 {
        chain_id: DEVNET_CHAIN_ID,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence: sequence,
        expiry: shape.expiry,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 1_000_000_000,
        gas_limit: TX_GAS_LIMIT_8130,
        account_changes: Vec::new(),
        calls,
        metadata: shape.metadata.clone(),
        payer: shape.payer.map(|p| SENDERS[p].address()),
    };

    let sender = SENDERS[shape.sender];
    let sender_auth: Bytes =
        sender.signer().sign_hash_sync(&tx.sender_signature_hash())?.as_bytes().to_vec().into();
    let payer_auth: Bytes = match shape.payer {
        // Explicit payer auth is `authenticator(20) || data`; for the K1 authenticator
        // the data is the payer's 65-byte signature over the payer digest.
        Some(p) => {
            let sig =
                SENDERS[p].signer().sign_hash_sync(&tx.payer_signature_hash(sender.address()))?;
            let mut auth = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
            auth.extend_from_slice(&sig.as_bytes());
            auth.into()
        }
        None => Bytes::new(),
    };

    Ok(Eip8130Signed::new(tx, sender_auth, payer_auth).encoded_2718().into())
}

/// Batch-encode ↔ derive round-trip over Base's own span-batch codec
/// (`base_protocol`), the other chain-halting agreement boundary: the batcher
/// encodes L2 blocks into a span batch and the derivation pipeline must
/// reconstruct exactly those blocks. A mismatch splits the chain.
///
/// This drives the real codec end to end, in-process (no L1, no devnet): fuzzed
/// user transactions (a mix of 1559/2930 and Base's EIP-8130) are grouped into
/// blocks, appended into a [`SpanBatch`] the way the batcher does
/// (`append_singular_batch`), serialized (`to_raw_span_batch` → `encode`), then
/// read back the way derivation does (`RawSpanBatch::decode` → `derive`), and
/// the reconstructed blocks must match the originals — transactions byte-for-
/// byte, plus epoch and timestamp. Deposits are excluded, as they are never
/// carried in span batches. Deterministic per seed; override with `FUZZ_SEED`.
#[test]
fn fuzz_derive_roundtrip() -> Result<()> {
    let seed = seed_from_env();
    let mut mix = StdRng::seed_from_u64(seed);
    let mut tx_gen = FuzzTxGenerator::new(seed);
    let mut tx8130_gen = Eip8130Generator::new(seed ^ 0x8130);
    let mut nonces = [0u64; SENDERS.len()];
    let mut sequences = [0u64; SENDERS.len()];
    // Calls target a funded EOA; the codec round-trip is independent of state, so
    // no contract needs to be deployed here.
    let recipient = SENDERS[0].address();

    // The per-block user transaction sets (no deposits — those are never span-batched).
    let mut block_txs: Vec<Vec<Bytes>> = Vec::with_capacity(DERIVE_BLOCKS);
    for _ in 0..DERIVE_BLOCKS {
        let mut txs = Vec::with_capacity(DERIVE_TXS_PER_BLOCK);
        for _ in 0..DERIVE_TXS_PER_BLOCK {
            // ~1/3 EIP-8130 (Base-specific span-batch tx data), the rest 1559/2930.
            let raw = if mix.random_range(0..3) == 0 {
                let shape = tx8130_gen.next_shape();
                let raw = sign_8130(&shape, sequences[shape.sender])?;
                sequences[shape.sender] += 1;
                raw
            } else {
                let shape = tx_gen.next_shape(recipient);
                let raw = sign_tx(&SENDERS[shape.sender], &shape, nonces[shape.sender])?;
                nonces[shape.sender] += 1;
                raw
            };
            txs.push(raw);
        }
        block_txs.push(txs);
    }

    // Encode side (batcher): append each block as a singular batch on a single
    // fixed L1 origin, with timestamps on the block-time grid so derive reproduces
    // them exactly.
    let parent_hash = B256::repeat_byte(0x11);
    let epoch_hash = B256::repeat_byte(0x22);
    let mut span = SpanBatch {
        chain_id: DEVNET_CHAIN_ID,
        genesis_timestamp: DERIVE_GENESIS_TS,
        ..Default::default()
    };
    for (i, txs) in block_txs.iter().enumerate() {
        let single = SingleBatch {
            parent_hash,
            epoch_num: DERIVE_EPOCH,
            epoch_hash,
            timestamp: DERIVE_GENESIS_TS + DERIVE_BLOCK_TIME * (i as u64 + 1),
            transactions: txs.clone(),
        };
        span.append_singular_batch(single, i as u64)
            .map_err(|e| eyre!("span batch append failed at block {i} (seed={seed:#x}): {e:?}"))?;
    }

    // Serialize, then read back exactly as the derivation pipeline does.
    let raw = span
        .to_raw_span_batch()
        .map_err(|e| eyre!("to_raw_span_batch failed (seed={seed:#x}): {e:?}"))?;
    let mut buf = Vec::new();
    raw.encode(&mut buf).map_err(|e| eyre!("span batch encode failed (seed={seed:#x}): {e:?}"))?;
    let mut decoded = RawSpanBatch::decode(&mut buf.as_slice())
        .map_err(|e| eyre!("span batch decode failed (seed={seed:#x}): {e:?}"))?;
    let derived = decoded
        .derive(DERIVE_BLOCK_TIME, DERIVE_GENESIS_TS, DEVNET_CHAIN_ID)
        .map_err(|e| eyre!("span batch derive failed (seed={seed:#x}): {e:?}"))?;

    // Derivation must reconstruct exactly what the batcher encoded.
    if derived.batches.len() != block_txs.len() {
        return Err(eyre!(
            "derive round-trip block count mismatch (seed={seed:#x}): {} != {}",
            derived.batches.len(),
            block_txs.len()
        ));
    }
    for (i, (element, original)) in derived.batches.iter().zip(block_txs.iter()).enumerate() {
        if element.transactions != *original {
            return Err(eyre!(
                "batch-encode/derive divergence at block {i} (seed={seed:#x}): \
                 encoded {} transactions, derived {} that do not match byte-for-byte",
                original.len(),
                element.transactions.len()
            ));
        }
        let expected_ts = DERIVE_GENESIS_TS + DERIVE_BLOCK_TIME * (i as u64 + 1);
        if element.timestamp != expected_ts {
            return Err(eyre!(
                "derive round-trip timestamp mismatch at block {i} (seed={seed:#x}): \
                 {} != {expected_ts}",
                element.timestamp
            ));
        }
        if element.epoch_num != DERIVE_EPOCH {
            return Err(eyre!(
                "derive round-trip epoch mismatch at block {i} (seed={seed:#x}): {} != {DERIVE_EPOCH}",
                element.epoch_num
            ));
        }
    }

    Ok(())
}
