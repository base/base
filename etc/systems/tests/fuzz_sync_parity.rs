//! Build/validate sync-parity fuzz test: fuzzed transactions through the devnet, with the
//! validator's sync to the sequencer as the oracle.
//!
//! This exercises agreement between block building and block validation: the
//! sequencer builds and gossips blocks, and the following validator must stay in
//! sync. If a fuzzed transaction set makes the sequencer build a block the
//! validator rejects as invalid, the validator stalls and this test fails.
//!
//! Transaction generation reuses the load tester's `WorkloadGenerator` (the paved
//! road for valid, signed, funded transactions) across a mix of transfer and
//! calldata payloads, submitted from several funded senders for block variety.
//! The adversarial permutation layer (access-list and gas-boundary variety, tx
//! reordering) is the next increment and slots in behind `FuzzTxGenerator`.
//!
//! The parity decision is factored into the pure `classify_parity` function so it
//! can be unit-tested deterministically (see the tests at the bottom); the live
//! test then drives it against the real devnet.
//!
//! The seed defaults to a fixed value locally and is overridable with `FUZZ_SEED`
//! (the nightly workflow picks a random seed per run); it is printed on failure so
//! any divergence replays.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_load_tests::{CalldataPayload, TransferPayload, WorkloadConfig, WorkloadGenerator};
use base_system_tests::{
    ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4, Account,
    SystemTestStackBuilder,
};
use eyre::{Result, WrapErr, eyre};
use tokio::time::{sleep, timeout};
use tracing::info;

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;

/// Default seed so a failing run replays deterministically. Override with the
/// `FUZZ_SEED` env var to explore more of the input space across runs.
const DEFAULT_SEED: u64 = 0x_BA5E_F0FF;
/// Number of fuzzed transactions to submit in one run.
const NUM_TXS: u64 = 200;
/// Gas limit applied to every generated transaction. Generous so calldata
/// payloads are always includable; the generator controls the interesting bits.
const TX_GAS_LIMIT: u64 = 200_000;
/// Max attempts per tx while the txpool is full, so submissions pace to block
/// production instead of bouncing off a full pool.
const MAX_SEND_ATTEMPTS: u32 = 20;
const SEND_BACKOFF: Duration = Duration::from_millis(250);

/// How far the validator may lag the sequencer and still count as "in sync".
/// The sequencer legitimately leads by a block or two, so this is not zero.
const MAX_LAG: u64 = 2;
/// Consecutive in-sync polls required before we declare parity. Guards against a
/// single lucky sample while the validator is actually stalling.
const PARITY_WINDOW: u32 = 5;
const PARITY_POLL_INTERVAL: Duration = Duration::from_millis(500);
const PARITY_TIMEOUT: Duration = Duration::from_secs(90);
const BALANCE_SYNC_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn fuzz_sync_parity() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();
    let seed = seed_from_env();

    // 1. Boot the devnet: sequencer (builder) + gossip-following validator (client).
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;
    let sequencer = system.l2_builder_provider()?;
    let validator = system.l2_client_provider()?;

    // 2. Submit a seeded, fuzzed transaction stream to the sequencer.
    let mut generator = FuzzTxGenerator::new(seed);
    let submitted =
        submit_fuzzed_txs(&sequencer, &validator, &mut generator, NUM_TXS, seed).await?;
    info!(seed, submitted, "submitted fuzzed transactions");

    // Guard against a vacuous pass: if the sequencer accepted (almost) nothing, the
    // parity check would succeed trivially without exercising the divergence surface.
    assert!(
        submitted >= NUM_TXS / 2,
        "too few transactions accepted (seed={seed:#x}): {submitted}/{NUM_TXS}; parity would pass vacuously"
    );

    // 3. Oracle: the validator must follow the sequencer to a shared head.
    assert_sync_parity(&sequencer, &validator, seed).await?;
    info!(seed, "validator stayed in sync with sequencer");

    Ok(())
}

fn seed_from_env() -> u64 {
    // Unset -> default (normal nightly). Set to a valid value -> that seed. Set to
    // garbage -> panic, so a mistyped workflow_dispatch seed fails loudly instead
    // of silently running (and reporting success) against the default seed.
    match std::env::var("FUZZ_SEED") {
        Ok(s) if !s.is_empty() => s
            .strip_prefix("0x")
            .map_or_else(|| s.parse(), |h| u64::from_str_radix(h, 16))
            .unwrap_or_else(|_| panic!("FUZZ_SEED={s:?} is not a valid u64")),
        _ => DEFAULT_SEED,
    }
}

/// Funded senders used to submit the fuzzed stream. Anvil accounts 1-4 are not
/// bound to protocol roles (5-9 are), so their nonces are ours to drive.
fn senders() -> [Account; 4] {
    [*ANVIL_ACCOUNT_1, *ANVIL_ACCOUNT_2, *ANVIL_ACCOUNT_3, *ANVIL_ACCOUNT_4]
}

/// Seeded generator for the fuzzed transaction stream, backed by the load
/// tester's `WorkloadGenerator`.
///
/// The adversarial permutation layer (access lists, gas boundaries, tx-type mix,
/// reordering) is the next increment and lives behind `next_shape`.
struct FuzzTxGenerator {
    generator: WorkloadGenerator,
}

impl FuzzTxGenerator {
    fn new(seed: u64) -> Self {
        let generator =
            WorkloadGenerator::new(WorkloadConfig::new("sync-parity-fuzz").with_seed(seed))
                .with_payload(TransferPayload::default(), 0.7)
                .with_payload(CalldataPayload::new(256).with_min_size(0), 0.3);
        Self { generator }
    }

    /// Produce the next transaction shape: recipient, value, and calldata.
    /// `from`, `nonce`, gas, and fees are completed by the caller (the paved-road
    /// "semantic completion" step) before signing.
    fn next_shape(&mut self, from: Address, to: Address) -> Result<FuzzedTx> {
        let request = self
            .generator
            .generate_payload(from, to)
            .map_err(|e| eyre!("workload generation failed: {e}"))?;
        Ok(FuzzedTx {
            to,
            value: request.value.unwrap_or(U256::ZERO),
            input: request.input.input().cloned().unwrap_or_default(),
        })
    }
}

/// A generated transaction shape, before semantic completion and signing.
struct FuzzedTx {
    to: Address,
    value: U256,
    input: Bytes,
}

/// Sign the fuzzed stream from several funded accounts and submit to the
/// sequencer. Returns the count actually accepted by the RPC.
async fn submit_fuzzed_txs(
    sequencer: &RootProvider<Base>,
    validator: &RootProvider<Base>,
    generator: &mut FuzzTxGenerator,
    count: u64,
    seed: u64,
) -> Result<u64> {
    // Prepare a signer and starting nonce for each sender.
    let sender_accounts = senders();
    let mut signers = Vec::new();
    for account in sender_accounts {
        let signer = signer_for(&account)?;
        wait_for_balance(validator, signer.address()).await?;
        let nonce = sequencer.get_transaction_count(signer.address()).await?;
        signers.push((signer, nonce));
    }

    let mut accepted = 0;
    for i in 0..count {
        let slot = (i as usize) % signers.len();
        // Recipient rotates through the other senders so funds stay in-system.
        let recipient = sender_accounts[(slot + 1) % sender_accounts.len()].address;
        let (signer, nonce) = &mut signers[slot];
        let current = *nonce;
        *nonce += 1;
        let shape = generator.next_shape(signer.address(), recipient)?;
        let raw = complete_and_sign(signer, &shape, current)?;
        if send_with_backoff(sequencer, &raw, current, seed).await {
            accepted += 1;
        } else {
            // Revert the nonce so this sender's subsequent txs don't hit a gap and
            // cascade into rejections unrelated to builder/validator parity.
            signers[slot].1 = current;
        }
    }
    Ok(accepted)
}

/// Submit one signed transaction, backing off and retrying while the txpool is
/// full (pure backpressure, not a rejection). Other errors are logged as
/// datapoints and skipped: the oracle is whether the validator follows whatever
/// the sequencer builds, not whether every tx is accepted.
async fn send_with_backoff(
    provider: &RootProvider<Base>,
    raw: &Bytes,
    nonce: u64,
    seed: u64,
) -> bool {
    for _ in 0..MAX_SEND_ATTEMPTS {
        match provider.send_raw_transaction(raw).await {
            Ok(_) => return true,
            Err(err) if err.to_string().contains("txpool is full") => {
                sleep(SEND_BACKOFF).await;
            }
            Err(err) => {
                info!(seed, nonce, error = %err, "tx rejected");
                return false;
            }
        }
    }
    info!(seed, nonce, "tx dropped after backoff (pool stayed full)");
    false
}

fn signer_for(account: &Account) -> Result<PrivateKeySigner> {
    Ok(PrivateKeySigner::from_bytes(&account.private_key)?)
}

/// Complete a fuzzed shape into a signed, EIP-2718 encoded transaction. This is
/// the "semantic completion" step: fill from/nonce/gas/fees/chain-id, then sign.
fn complete_and_sign(signer: &PrivateKeySigner, tx: &FuzzedTx, nonce: u64) -> Result<Bytes> {
    let request = BaseTransactionRequest::default()
        .from(signer.address())
        .to(tx.to)
        .value(tx.value)
        .with_input(tx.input.clone())
        .transaction_type(2)
        .with_gas_limit(TX_GAS_LIMIT)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(L2_CHAIN_ID)
        .with_nonce(nonce);
    let typed =
        request.build_typed_tx().map_err(|req| eyre!("invalid transaction request: {req:?}"))?;
    let signature = signer.sign_hash_sync(&typed.signature_hash())?;
    Ok(typed.into_signed(signature).encoded_2718().into())
}

/// The oracle. Requires the validator to catch the sequencer head and agree on
/// block hashes at a shared height, sustained across `PARITY_WINDOW` polls.
///
/// A stalled validator (divergence) never closes the gap and this
/// times out; a silent fork is caught by the block-hash comparison.
async fn assert_sync_parity(
    sequencer: &RootProvider<Base>,
    validator: &RootProvider<Base>,
    seed: u64,
) -> Result<()> {
    let outcome = timeout(PARITY_TIMEOUT, async {
        let mut in_sync_streak = 0u32;
        loop {
            let seq_head = sequencer.get_block_number().await?;
            let val_head = validator.get_block_number().await?;

            // Height both nodes should have; used to compare hashes.
            let shared = seq_head.saturating_sub(MAX_LAG);
            let (seq_hash, val_hash) = if val_head >= shared && val_head > 0 {
                (block_hash(sequencer, shared).await?, block_hash(validator, shared).await.ok())
            } else {
                (block_hash(sequencer, shared).await?, None)
            };

            match classify_parity(seq_head, val_head, seq_hash, val_hash, MAX_LAG) {
                Parity::InSync => {
                    in_sync_streak += 1;
                    if in_sync_streak >= PARITY_WINDOW {
                        return Ok::<_, eyre::Error>(seq_head);
                    }
                }
                Parity::Forked => {
                    return Err(eyre!(
                        "fork at block {shared}: sequencer {seq_hash} != validator {val_hash:?}"
                    ));
                }
                Parity::Lagging => in_sync_streak = 0,
            }
            sleep(PARITY_POLL_INTERVAL).await;
        }
    })
    .await;

    match outcome {
        Ok(Ok(head)) => {
            info!(seed, head, "reached sync parity");
            Ok(())
        }
        Ok(Err(fork)) => Err(fork.wrap_err(format!("divergence detected (seed={seed:#x})"))),
        Err(_) => {
            let seq_head = sequencer.get_block_number().await.unwrap_or_default();
            let val_head = validator.get_block_number().await.unwrap_or_default();
            Err(eyre!(
                "validator failed to stay in sync (seed={seed:#x}): sequencer at {seq_head}, \
                 validator stalled at {val_head}"
            ))
        }
    }
}

/// Parity verdict for a single poll. Pure so it can be unit-tested.
#[derive(Debug, PartialEq, Eq)]
enum Parity {
    /// Validator is within `max_lag` of the sequencer and hashes agree.
    InSync,
    /// Validator is too far behind (or has not started following).
    Lagging,
    /// Validator is caught up in height but on a different block hash.
    Forked,
}

/// Decide the parity verdict from the two heads and the hashes at a shared height.
/// `val_hash` is `None` when the validator does not yet have the shared block.
fn classify_parity(
    seq_head: u64,
    val_head: u64,
    seq_hash: B256,
    val_hash: Option<B256>,
    max_lag: u64,
) -> Parity {
    if seq_head.saturating_sub(val_head) > max_lag || val_head == 0 {
        return Parity::Lagging;
    }
    match val_hash {
        Some(hash) if hash == seq_hash => Parity::InSync,
        Some(_) => Parity::Forked,
        None => Parity::Lagging,
    }
}

/// Fetch the hash of a block by number, erroring if the node does not have it yet.
async fn block_hash(provider: &RootProvider<Base>, number: u64) -> Result<B256> {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await?
        .ok_or_else(|| eyre!("block {number} missing"))?;
    Ok(block.header.hash)
}

/// Wait until the funded sender's balance has propagated to a node.
async fn wait_for_balance(provider: &RootProvider<Base>, address: Address) -> Result<()> {
    timeout(BALANCE_SYNC_TIMEOUT, async {
        loop {
            if provider.get_balance(address).await? > U256::ZERO {
                return Ok::<_, eyre::Error>(());
            }
            sleep(PARITY_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("timed out waiting for sender balance to sync")?
}

#[cfg(test)]
mod tests {
    use super::*;

    const H1: B256 = B256::repeat_byte(0x11);
    const H2: B256 = B256::repeat_byte(0x22);

    #[test]
    fn in_sync_when_within_lag_and_hashes_agree() {
        assert_eq!(classify_parity(10, 9, H1, Some(H1), MAX_LAG), Parity::InSync);
        assert_eq!(classify_parity(10, 10, H1, Some(H1), MAX_LAG), Parity::InSync);
    }

    #[test]
    fn lagging_when_validator_falls_behind() {
        // Divergence: sequencer advances, validator stalls.
        assert_eq!(classify_parity(50, 10, H1, Some(H1), MAX_LAG), Parity::Lagging);
        // Validator has not started following yet.
        assert_eq!(classify_parity(10, 0, H1, None, MAX_LAG), Parity::Lagging);
        // Caught up in height but missing the shared block.
        assert_eq!(classify_parity(10, 9, H1, None, MAX_LAG), Parity::Lagging);
    }

    #[test]
    fn forked_when_heights_match_but_hashes_differ() {
        assert_eq!(classify_parity(10, 9, H1, Some(H2), MAX_LAG), Parity::Forked);
    }
}
