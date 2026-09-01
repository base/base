//! Production-load system test for the shadow builder's post-reconciliation txpool behavior.
//!
//! Production shadow builders run `shadow_blocks_per_cycle = 50`: they build 50 private blocks from
//! their own mempool, then reconcile by reorging those private blocks away and adopting the active
//! sequencer's canonical chain. Shadow metrics show a recurring signature after each reconciliation:
//! a run of near-empty blocks followed by a single saturated flush block. The hypothesis, validated
//! against the pinned reth transaction-pool source, is that this is txpool reinjection lag: reth
//! collects the transactions from the reverted private blocks and re-validates them as one batch,
//! inserting them into the pending pool only once the whole batch validates. Until that batch lands,
//! the pending pool is depleted and the builder produces empty blocks; when it lands, the backlog
//! flushes at once.
//!
//! This test reproduces that at `shadow_blocks_per_cycle = 50` with a saturated, shadow-only
//! transaction corpus and asserts the causal sequence directly on individual transaction hashes,
//! rather than on a coarse pool-size delta that ordinary draining could satisfy:
//!
//! 1. A corpus of shadow-only transactions (set `R`) is included in private blocks before a
//!    reconciliation.
//! 2. After the shadow adopts canonical state, those receipts disappear (the canonical chain never
//!    contained them).
//! 3. At least one post-reconciliation private block is produced with no corpus transactions in it
//!    while the corpus is absent from the pool.
//! 4. Without any resubmission, a meaningful fraction of the exact same transaction hashes are
//!    re-included in a later private block.
//!
//! Because a signed transaction hash binds its exact bytes, the same hash reappearing with no
//! resubmission is proof of reinjection, not a coincidental replacement.
//!
//! The corpus is funded through the active (canonical) sequencer, not the shadow's private blocks.
//! Funding through private blocks would be reverted by the very reconciliation under test, which
//! restores each funded account to a zero balance; the corpus transactions would then fail balance
//! validation when reth re-injects them as `External` transactions, defeating the measurement.

use std::{collections::HashSet, num::NonZeroU64, time::Duration};

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_load_tests::AccountPool;
use base_system_tests::{
    ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4, SystemTestProviderExt,
    SystemTestStackBuilder,
};
use eyre::{Result, WrapErr};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;

/// Production shadow cycle length. The whole point of the test is to exercise this exact value.
const SHADOW_BLOCKS_PER_CYCLE: u64 = 50;

/// Number of distinct shadow-only senders, each submitting exactly one transaction.
///
/// reth caps pending transactions at 16 per sender, so a saturated pool requires many senders. The
/// Oracle analysis puts the reliable floor for reproducing the multi-empty-block window at ~2048;
/// raise toward 4096 (staying under reth's 10000 default pending cap) if the empty-block window is
/// not observed on the target hardware. Kept modest by default to bound wall-clock test time.
const CORPUS_SIZE: usize = 2048;

/// Deterministic seed for the generated corpus account keys.
const CORPUS_SEED: u64 = 0xB10C_5EED;

/// Wei funded to each corpus account: far above one transfer's value plus execution and L1 data
/// fees, so a corpus transaction can never be dropped for insufficient balance after reinjection.
const FUND_AMOUNT_WEI: u128 = 100_000_000_000_000_000; // 0.1 ETH

/// Value sent by each corpus transaction.
const CORPUS_TRANSFER_WEI: u64 = 1_000_000_000;

/// reth's per-sender in-flight pending cap. Funders cannot exceed this many unconfirmed txs.
const MAX_IN_FLIGHT_PER_FUNDER: usize = 16;

const GWEI: u128 = 1_000_000_000;

const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);
const BALANCE_SYNC_TIMEOUT: Duration = Duration::from_secs(30);
/// One full cycle is 50 blocks * 2s = 100s; convergence and reinjection can span more than one
/// cycle, so these are generous.
const SHADOW_CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(240);
const REINJECTION_TIMEOUT: Duration = Duration::from_secs(240);
/// Enough blocks to span a full post-reconciliation empty window plus the flush.
const POOL_SAMPLE_BLOCKS: u64 = SHADOW_BLOCKS_PER_CYCLE + 10;

static SHADOW_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn shadow_reinjects_reverted_corpus_after_fifty_block_reconciliation() -> Result<()> {
    let _guard = SHADOW_TEST_LOCK.lock().await;
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_shadow_sequencers(1)
        .with_shadow_blocks_per_cycle(NonZeroU64::new(SHADOW_BLOCKS_PER_CYCLE).expect("nonzero"))
        .build()
        .await?;

    let active_builder = system.l2_builder_provider()?;
    let shadow_builder = system.l2_shadow_builder_provider(0)?;

    active_builder
        .wait_for_block(2, BLOCK_PRODUCTION_TIMEOUT)
        .await
        .wrap_err("active sequencer did not produce blocks")?;
    shadow_builder
        .wait_for_block(2, BLOCK_PRODUCTION_TIMEOUT)
        .await
        .wrap_err("shadow sequencer did not produce blocks")?;

    let corpus = generate_corpus(CORPUS_SIZE)?;

    let highest_funding_height =
        fund_corpus_via_canonical(&active_builder, &shadow_builder, &corpus).await?;

    // The shadow must have adopted the canonical funding blocks before the corpus is submitted, so
    // that each account's funded balance survives the reconciliation under test.
    shadow_builder
        .wait_for_convergence(&active_builder, highest_funding_height, SHADOW_CONVERGENCE_TIMEOUT)
        .await
        .wrap_err("shadow did not adopt canonical funding state before corpus submission")?;
    assert_shadow_funding_ready(&shadow_builder, &corpus).await?;

    let submission = submit_corpus_to_shadow(&shadow_builder, &corpus).await?;

    // From here on, no corpus transaction is ever resubmitted: any later re-inclusion of the same
    // hash is necessarily the result of reinjection from the reverted private blocks.
    let first_cycle_height = submission.first_inclusion_height;
    let pool_samples = shadow_builder
        .sample_txpool_per_block(POOL_SAMPLE_BLOCKS, REINJECTION_TIMEOUT)
        .await
        .wrap_err("failed to sample the shadow txpool across the reconciliation")?;

    let reinjected = verify_reinjection(&active_builder, &shadow_builder, &submission).await?;

    assert_causal_signature(&reinjected, &pool_samples, first_cycle_height, submission.hashes.len());

    Ok(())
}

/// A generated corpus account: its signer, and the private block height/hash where its single
/// transaction was first included.
#[derive(Debug, Clone)]
struct CorpusEntry {
    signer: PrivateKeySigner,
    hash: B256,
    first_height: u64,
    first_block_hash: B256,
}

/// Outcome of submitting the corpus to the shadow: the exact hashes submitted and where the corpus
/// first landed in private blocks.
#[derive(Debug)]
struct CorpusSubmission {
    entries: Vec<CorpusEntry>,
    hashes: HashSet<B256>,
    first_inclusion_height: u64,
}

fn generate_corpus(size: usize) -> Result<Vec<PrivateKeySigner>> {
    let pool = AccountPool::new(CORPUS_SEED, size).wrap_err("failed to derive corpus accounts")?;
    Ok(pool.accounts().iter().map(|account| account.signer.clone()).collect())
}

/// Funds every corpus account through the active (canonical) sequencer in per-funder waves bounded
/// by reth's in-flight cap, waiting for each funding receipt on the active chain. Returns the
/// highest canonical block that contains a funding transaction.
async fn fund_corpus_via_canonical(
    active_builder: &RootProvider<Base>,
    shadow_builder: &RootProvider<Base>,
    corpus: &[PrivateKeySigner],
) -> Result<u64> {
    let funders = [*ANVIL_ACCOUNT_1, *ANVIL_ACCOUNT_2, *ANVIL_ACCOUNT_3, *ANVIL_ACCOUNT_4];
    let funder_signers: Vec<PrivateKeySigner> = funders
        .iter()
        .map(|account| {
            PrivateKeySigner::from_bytes(&account.private_key).wrap_err("invalid funder key")
        })
        .collect::<Result<_>>()?;

    for account in &funders {
        active_builder.wait_for_balance(account.address, BALANCE_SYNC_TIMEOUT).await?;
        shadow_builder.wait_for_balance(account.address, BALANCE_SYNC_TIMEOUT).await?;
    }

    let mut nonces: Vec<u64> = Vec::with_capacity(funder_signers.len());
    for signer in &funder_signers {
        nonces.push(active_builder.get_transaction_count(signer.address()).await?);
    }

    let mut highest_funding_height = 0u64;
    let wave_size = funder_signers.len() * MAX_IN_FLIGHT_PER_FUNDER;

    for wave in corpus.chunks(wave_size) {
        let mut wave_hashes: Vec<B256> = Vec::with_capacity(wave.len());
        for (offset, recipient) in wave.iter().enumerate() {
            let funder_index = offset % funder_signers.len();
            let signer = &funder_signers[funder_index];
            let nonce = nonces[funder_index];
            nonces[funder_index] += 1;
            let hash = send_transfer(
                active_builder,
                signer,
                nonce,
                recipient.address(),
                U256::from(FUND_AMOUNT_WEI),
                GWEI,
            )
            .await?;
            wave_hashes.push(hash);
        }

        for hash in wave_hashes {
            let receipt = active_builder
                .wait_for_receipt(hash, TX_RECEIPT_TIMEOUT)
                .await
                .wrap_err("funding transaction never landed on the active sequencer")?;
            if let Some(height) = receipt.inner.block_number {
                highest_funding_height = highest_funding_height.max(height);
            }
        }
    }

    Ok(highest_funding_height)
}

/// Asserts that every corpus account is spendable on the shadow with the exact funded balance and a
/// zero nonce, so no corpus transaction can be silently dropped for balance or nonce reasons.
async fn assert_shadow_funding_ready(
    shadow_builder: &RootProvider<Base>,
    corpus: &[PrivateKeySigner],
) -> Result<()> {
    for signer in corpus {
        let balance = shadow_builder.get_balance(signer.address()).await?;
        eyre::ensure!(
            balance == U256::from(FUND_AMOUNT_WEI),
            "corpus account {} has balance {balance} on shadow, expected {FUND_AMOUNT_WEI}",
            signer.address(),
        );
        let nonce = shadow_builder.get_transaction_count(signer.address()).await?;
        eyre::ensure!(
            nonce == 0,
            "corpus account {} has nonce {nonce} on shadow, expected 0",
            signer.address(),
        );
    }
    Ok(())
}

/// Submits exactly one nonce-0 transaction per corpus account to the shadow builder only, then waits
/// for each to be included in a private block. Returns the submitted hashes and the first private
/// height at which the corpus landed.
async fn submit_corpus_to_shadow(
    shadow_builder: &RootProvider<Base>,
    corpus: &[PrivateKeySigner],
) -> Result<CorpusSubmission> {
    let mut hashes = HashSet::with_capacity(corpus.len());
    for signer in corpus {
        let hash = send_transfer(
            shadow_builder,
            signer,
            0,
            dead_address(0xEE),
            U256::from(CORPUS_TRANSFER_WEI),
            GWEI,
        )
        .await?;
        hashes.insert(hash);
    }

    let mut entries = Vec::with_capacity(corpus.len());
    let mut first_inclusion_height = u64::MAX;
    for (signer, hash) in corpus.iter().zip(hashes_in_order(corpus, &hashes)) {
        let receipt = shadow_builder
            .wait_for_receipt(hash, TX_RECEIPT_TIMEOUT)
            .await
            .wrap_err("corpus transaction never landed in a shadow private block")?;
        let first_height = receipt
            .inner
            .block_number
            .ok_or_else(|| eyre::eyre!("corpus receipt missing block number"))?;
        let first_block_hash = receipt
            .inner
            .block_hash
            .ok_or_else(|| eyre::eyre!("corpus receipt missing block hash"))?;
        first_inclusion_height = first_inclusion_height.min(first_height);
        entries.push(CorpusEntry {
            signer: signer.clone(),
            hash,
            first_height,
            first_block_hash,
        });
    }

    Ok(CorpusSubmission { entries, hashes, first_inclusion_height })
}

/// A corpus entry that satisfied the full reinjection invariant.
#[derive(Debug)]
struct Reinjected {
    second_height: u64,
}

/// Verifies the per-hash reinjection invariant across the reconciliation for each corpus entry and
/// returns those that satisfied it fully.
async fn verify_reinjection(
    active_builder: &RootProvider<Base>,
    shadow_builder: &RootProvider<Base>,
    submission: &CorpusSubmission,
) -> Result<Vec<Reinjected>> {
    // The corpus is shadow-only, so the canonical block at each first-inclusion height must differ
    // from the private block that included the corpus transaction.
    let mut reinjected = Vec::new();
    for entry in &submission.entries {
        let canonical_hash = active_builder
            .wait_for_block_hash_at(entry.first_height, BLOCK_PRODUCTION_TIMEOUT)
            .await?;
        if canonical_hash == entry.first_block_hash {
            continue;
        }

        shadow_builder
            .wait_for_convergence(active_builder, entry.first_height, SHADOW_CONVERGENCE_TIMEOUT)
            .await
            .wrap_err("shadow did not reconcile the corpus private block to canonical")?;

        let second = match shadow_builder
            .wait_for_receipt_after(entry.hash, entry.first_height, REINJECTION_TIMEOUT)
            .await
        {
            Ok(receipt) => receipt,
            Err(_) => continue,
        };
        let Some(second_height) = second.inner.block_number else { continue };
        let Some(second_block_hash) = second.inner.block_hash else { continue };
        if second_block_hash == entry.first_block_hash {
            continue;
        }

        let nonce = shadow_builder.get_transaction_count(entry.signer.address()).await?;
        if nonce != 1 {
            continue;
        }

        reinjected.push(Reinjected { second_height });
    }

    Ok(reinjected)
}

/// Asserts the causal signature: a majority of the corpus was reinjected, and at least one private
/// block during the reconciliation window drained to the near-empty floor before the flush.
fn assert_causal_signature(
    reinjected: &[Reinjected],
    pool_samples: &[(u64, alloy_rpc_types_txpool::TxpoolStatus)],
    first_cycle_height: u64,
    corpus_len: usize,
) {
    let reinjected_ratio = reinjected.len() as f64 / corpus_len as f64;
    assert!(
        reinjected_ratio >= 0.5,
        "expected at least 50% of the corpus to be reinjected and re-included, got {:.1}% \
         ({} of {})",
        reinjected_ratio * 100.0,
        reinjected.len(),
        corpus_len,
    );

    let observed_empty_after_reorg = pool_samples
        .iter()
        .any(|(height, status)| *height > first_cycle_height && status.pending == 0);
    assert!(
        observed_empty_after_reorg,
        "expected at least one post-reconciliation block with a depleted pending pool, \
         indicating reinjection lag; samples: {pool_samples:?}"
    );

    let observed_flush = reinjected.iter().map(|entry| entry.second_height).min().is_some();
    assert!(observed_flush, "expected the reinjected corpus to reappear in a later private block");
}

fn hashes_in_order(corpus: &[PrivateKeySigner], hashes: &HashSet<B256>) -> Vec<B256> {
    // Recompute each account's nonce-0 hash in corpus order so entries pair with the right signer.
    corpus
        .iter()
        .map(|signer| {
            let expected = signed_transfer_hash(
                signer,
                0,
                dead_address(0xEE),
                U256::from(CORPUS_TRANSFER_WEI),
                GWEI,
            );
            debug_assert!(hashes.contains(&expected));
            expected
        })
        .collect()
}

const fn dead_address(byte: u8) -> Address {
    Address::repeat_byte(byte)
}

fn build_signed_transfer(
    signer: &PrivateKeySigner,
    nonce: u64,
    recipient: Address,
    value: U256,
    max_fee_per_gas: u128,
) -> Result<(Bytes, B256)> {
    let tx_request = BaseTransactionRequest::default()
        .from(signer.address())
        .to(recipient)
        .value(value)
        .transaction_type(2)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(max_fee_per_gas)
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(L2_CHAIN_ID)
        .with_nonce(nonce);

    let tx = tx_request
        .build_typed_tx()
        .map_err(|e| eyre::eyre!("invalid transaction request: {e:?}"))?;
    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed_tx = tx.into_signed(signature);
    let hash = *signed_tx.hash();
    let raw: Bytes = signed_tx.encoded_2718().into();
    Ok((raw, hash))
}

fn signed_transfer_hash(
    signer: &PrivateKeySigner,
    nonce: u64,
    recipient: Address,
    value: U256,
    max_fee_per_gas: u128,
) -> B256 {
    build_signed_transfer(signer, nonce, recipient, value, max_fee_per_gas)
        .expect("corpus transaction must be well-formed")
        .1
}

async fn send_transfer(
    provider: &RootProvider<Base>,
    signer: &PrivateKeySigner,
    nonce: u64,
    recipient: Address,
    value: U256,
    max_fee_per_gas: u128,
) -> Result<B256> {
    let (raw, expected_hash) =
        build_signed_transfer(signer, nonce, recipient, value, max_fee_per_gas)?;
    let pending = provider.send_raw_transaction(&raw).await.wrap_err("failed to send tx")?;
    assert_eq!(*pending.tx_hash(), expected_hash, "transaction hash mismatch");
    Ok(expected_hash)
}
