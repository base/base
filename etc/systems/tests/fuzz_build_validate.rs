//! Deterministic in-process build/validate differential fuzz test.
//!
//! Builds blocks from fuzzed transaction sets on a single in-process node and
//! checks that the executor accepts what the payload builder produced.
//! `build_block_from_transactions` runs `getPayload` (the builder path) and then
//! `newPayload` (the validator path); an error there means the builder produced a
//! block the validator rejects, the class of disagreement that halts the chain.
//!
//! Unlike the sync-parity test, this runs fully in-process (no devnet, no Docker),
//! forces exactly the generated transactions into each block (`no_tx_pool`), and
//! uses a fixed per-block timestamp, so a run is deterministic and a failing seed
//! replays exactly. Override the seed with `FUZZ_SEED`.

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
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
    let harness = TestHarness::new().await?;

    let mut generator = FuzzTxGenerator::new(seed);
    let mut nonces = [0u64; SENDERS.len()];

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
    }

    Ok(())
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
        TxShape { sender, to, value, input }
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
        .with_nonce(nonce);
    let typed =
        request.build_typed_tx().map_err(|req| eyre!("invalid transaction request: {req:?}"))?;
    let signature = signer.sign_hash_sync(&typed.signature_hash())?;
    Ok(typed.into_signed(signature).encoded_2718().into())
}
