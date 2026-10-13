//! End-to-end latency harness for the mempool → builder ingestion path.
//!
//! Runs two in-process nodes: a mempool node with [`TxForwardingExtension`] and a builder node
//! serving `base_insertValidatedTransaction` through [`BuilderApiImpl`] over its real pool. Each
//! transaction is timestamped at every hop:
//!
//! - `submit`: the client sends `eth_sendRawTransaction` to the mempool node.
//! - `mempool_pending`: the mempool pool announces it as pending.
//! - `builder_recv`: the builder RPC has deserialized the forwarded request.
//! - `builder_inserted`: the builder's pool insert returned.
//! - `builder_pending`: the builder pool announces it as pending, which is when the build loop's
//!   best-transactions iterator can yield it.
//!
//! Ignored by default because it measures rather than asserts. Run with:
//!
//! ```sh
//! cargo test --release -p base-tx-forwarding --test latency -- --ignored --nocapture
//! ```

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::GenesisAccount;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Bytes, TxHash, U256, keccak256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::{
    BuilderApiImpl, BuilderApiServer, TransactionValidity, ValidatedTransaction,
};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, NodeHooks, test_utils::TestHarness};
use base_test_utils::{Account, DEVNET_CHAIN_ID, build_test_genesis};
use base_tx_forwarding::{TxForwardingConfig, TxForwardingExtension};
use eyre::Result;
use jsonrpsee::RpcModule;
use reth_transaction_pool::{TransactionListenerKind, TransactionPool};
use tokio::time::{sleep, timeout};
use url::Url;

const SENDERS: usize = 2_000;
const ISOLATED: usize = 200;
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);

type Stamps = Arc<Mutex<HashMap<TxHash, Instant>>>;

#[derive(Debug, Default, Clone)]
struct Probes {
    mempool_pending: Stamps,
    builder_recv: Stamps,
    builder_inserted: Stamps,
    builder_pending: Stamps,
}

fn stamp(stamps: &Stamps, hash: TxHash) {
    stamps.lock().unwrap().entry(hash).or_insert_with(Instant::now);
}

/// Spawns a task stamping every hash `pool` announces as pending.
fn watch_pending<P: TransactionPool + 'static>(pool: &P, stamps: Stamps) {
    let mut pending = pool.pending_transactions_listener_for(TransactionListenerKind::All);
    tokio::spawn(async move {
        while let Some(hash) = pending.recv().await {
            stamp(&stamps, hash);
        }
    });
}

/// Mempool-side probe: stamps pool pending announcements.
#[derive(Debug)]
struct MempoolProbe(Stamps);

impl BaseNodeExtension for MempoolProbe {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            watch_pending(ctx.pool(), self.0);
            Ok(())
        })
    }
}

/// Builder-side endpoint: the production handler wrapped with arrival and completion stamps.
#[derive(Debug)]
struct BuilderProbe(Probes);

impl BaseNodeExtension for BuilderProbe {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let probes = self.0;
            watch_pending(ctx.pool(), Arc::clone(&probes.builder_pending));
            let api = BuilderApiImpl::<_, TransactionValidity>::with_extensions(
                ctx.pool().clone(),
                true,
                usize::MAX,
            );
            let mut module = RpcModule::new((api, probes));
            module.register_async_method(
                "base_insertValidatedTransaction",
                |params, context, _| async move {
                    let tx: ValidatedTransaction<TransactionValidity> = params.one()?;
                    let hash = keccak256(&tx.raw);
                    stamp(&context.1.builder_recv, hash);
                    let result = context.0.insert_validated_transaction(tx).await;
                    stamp(&context.1.builder_inserted, hash);
                    result
                },
            )?;
            ctx.modules.merge_configured(module)?;
            Ok(())
        })
    }
}

fn signed_transfer(signer: &PrivateKeySigner, nonce: u64) -> Bytes {
    let request = BaseTransactionRequest::default()
        .from(signer.address())
        .transaction_type(2u8)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(10_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(DEVNET_CHAIN_ID)
        .to(Account::Bob.address())
        .with_nonce(nonce);
    let transaction = request.build_typed_tx().expect("valid transaction request");
    let signature = signer.sign_hash_sync(&transaction.signature_hash()).expect("signs");
    transaction.into_signed(signature).encoded_2718().into()
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn report(label: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    println!(
        "{label:<34} n={:<5} mean={:>9.3?} p50={:>9.3?} p90={:>9.3?} p99={:>9.3?} max={:>9.3?}",
        samples.len(),
        mean,
        percentile(&samples, 0.5),
        percentile(&samples, 0.9),
        percentile(&samples, 0.99),
        samples[samples.len() - 1],
    );
}

/// Prints per-hop latency for `hashes`, each submitted at the paired instant.
fn report_hops(probes: &Probes, submitted: &[(TxHash, Instant)]) {
    let mempool = probes.mempool_pending.lock().unwrap();
    let recv = probes.builder_recv.lock().unwrap();
    let inserted = probes.builder_inserted.lock().unwrap();
    let pending = probes.builder_pending.lock().unwrap();
    let hop = |from: &dyn Fn(&TxHash, &Instant) -> Instant, to: &HashMap<TxHash, Instant>| {
        submitted
            .iter()
            .map(|(hash, at)| to[hash].saturating_duration_since(from(hash, at)))
            .collect::<Vec<_>>()
    };
    report("submit -> mempool pending", hop(&|_, at| *at, &mempool));
    report("mempool pending -> builder recv", hop(&|hash, _| mempool[hash], &recv));
    report("builder recv -> insert returned", hop(&|hash, _| recv[hash], &inserted));
    report("builder recv -> builder pending", hop(&|hash, _| recv[hash], &pending));
    report("submit -> builder pending (e2e)", hop(&|_, at| *at, &pending));
}

async fn wait_for(stamps: &Stamps, hashes: impl IntoIterator<Item = TxHash>) -> Result<()> {
    let hashes: Vec<_> = hashes.into_iter().collect();
    timeout(WAIT_TIMEOUT, async {
        loop {
            {
                let stamps = stamps.lock().unwrap();
                if hashes.iter().all(|hash| stamps.contains_key(hash)) {
                    return;
                }
            }
            sleep(Duration::from_micros(100)).await;
        }
    })
    .await
    .map_err(|_| eyre::eyre!("transactions did not reach the builder pool"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "latency measurement; run explicitly with --ignored --nocapture"]
async fn ingestion_latency() -> Result<()> {
    let signers: Vec<_> = (0..SENDERS).map(|_| PrivateKeySigner::random()).collect();
    let mut genesis = build_test_genesis();
    for signer in &signers {
        genesis.alloc.insert(
            signer.address(),
            GenesisAccount::default().with_balance(U256::from(10u128.pow(24))),
        );
    }
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));

    let probes = Probes::default();
    let builder = TestHarness::builder()
        .with_extension(BuilderProbe(probes.clone()))
        .with_chain_spec(Arc::clone(&chain_spec))
        .build()
        .await?;
    let builder_url = Url::parse(&builder.rpc_url())?;
    let mempool = TestHarness::builder()
        .with_extension(MempoolProbe(Arc::clone(&probes.mempool_pending)))
        .with_ext::<TxForwardingExtension>(TxForwardingConfig::new(vec![builder_url]))
        .with_chain_spec(chain_spec)
        .build()
        .await?;
    let provider = mempool.provider();

    let raws: Vec<Bytes> = signers.iter().map(|signer| signed_transfer(signer, 0)).collect();
    let hashes: Vec<TxHash> = raws.iter().map(keccak256).collect();

    // Warm up both nodes' RPC, validation and forwarding paths.
    for raw in &raws[..20] {
        let _ = provider.send_raw_transaction(raw).await?;
    }
    wait_for(&probes.builder_pending, hashes[..20].iter().copied()).await?;

    // Isolated: one transaction in flight at a time, jittered so arrivals do not phase-lock
    // with any polling interval on the path.
    let mut submitted = Vec::with_capacity(ISOLATED);
    for (index, raw) in raws[20..20 + ISOLATED].iter().enumerate() {
        let hash = hashes[20 + index];
        let at = Instant::now();
        let _ = provider.send_raw_transaction(raw).await?;
        submitted.push((hash, at));
        wait_for(&probes.builder_pending, [hash]).await?;
        sleep(Duration::from_micros((index as u64 * 7_919) % 5_000)).await;
    }
    println!("\n== isolated ({ISOLATED} txs, one in flight) ==");
    report_hops(&probes, &submitted);

    // Burst: every remaining transaction submitted concurrently from 32 clients.
    let burst = &raws[20 + ISOLATED..];
    let burst_hashes = &hashes[20 + ISOLATED..];
    let started = Instant::now();
    let submitted = Arc::new(Mutex::new(Vec::with_capacity(burst.len())));
    let mut clients = Vec::new();
    for chunk in burst.iter().zip(burst_hashes).collect::<Vec<_>>().chunks(burst.len() / 32 + 1) {
        let chunk: Vec<_> = chunk.iter().map(|(raw, hash)| ((*raw).clone(), **hash)).collect();
        let provider = provider.clone();
        let submitted = Arc::clone(&submitted);
        clients.push(tokio::spawn(async move {
            for (raw, hash) in chunk {
                let at = Instant::now();
                let _ = provider.send_raw_transaction(&raw).await.expect("mempool accepts");
                submitted.lock().unwrap().push((hash, at));
            }
        }));
    }
    for client in clients {
        client.await?;
    }
    let accepted = started.elapsed();
    wait_for(&probes.builder_pending, burst_hashes.iter().copied()).await?;
    let drained = started.elapsed();
    println!("\n== burst ({} txs, 32 clients) ==", burst.len());
    println!(
        "mempool accepted all in {accepted:.3?} ({:.0} tx/s); builder pending all in {drained:.3?} \
         ({:.0} tx/s)",
        burst.len() as f64 / accepted.as_secs_f64(),
        burst.len() as f64 / drained.as_secs_f64(),
    );
    report_hops(&probes, &submitted.lock().unwrap());

    drop(mempool);
    drop(builder);
    Ok(())
}
