//! Phase 2: native payload-builder cost of considering insolvent EIP-8130 txs.
//!
//! Simulates the no-flashblocks sequencer job after a payer drain: the 8130
//! transactions are still in the candidate set, balances are already zero, and
//! the builder must skip them before it can finish the block.
//!
//! Times [`BasePayloadBuilderCtx::execute_best_transactions`] only (not state
//! root / seal), which is the work that has to fit in the 100ms build budget.
//!
//! `considered` counts the transactions that reached an affordability check; it must track
//! the number of drained payers, not the number of candidates, confirming one cheap check
//! per payer with no per-transaction revalidation as the backlog scales.
//!
//! Run measured sizes with:
//! `cargo test -p base-execution-payload-builder --test high_rate_insolvent_payload \
//!     phase2_insolvent_payload_bench --release -- --ignored --nocapture`
//!
//! Optional `PAYLOAD_BENCH_SIZES` (default `100,1000`); use `PAYLOAD_BENCH_SIZES=200000` for
//! the launch-scale pool pass.
//!
//! Deferred until a measured bottleneck at a higher per-payer cap justifies the complexity
//! (this build path stays cheap at cap=4): a build-deadline check inside long lazy-skip
//! runs, pool-level (cross-build) payer suspension, deferred/background reclamation of
//! suspended entries, payer generations, and a concurrent-admission harness for the 200k
//! pool. Cleanup stays a single bulk pass over the existing reverse indexes.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Instant,
};

use alloy_consensus::{Header, Transaction, transaction::Recovered};
use alloy_primitives::{Address, B64, B256, Bytes, StorageKey, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::ChainConfig;
use base_common_consensus::{
    BasePooledTransaction as ConsensusPooledTransaction, BasePrimitives, BaseTxEnvelope,
    Eip8130Constants, Eip8130Signed, Predeploys, TxEip8130,
};
use base_common_evm::BaseTime;
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, CoinbaseTipAffordability, ParkablePayloadTransactions,
    builder::{BasePayloadBuilderCtx, ExecutionInfo},
    config::BaseBuilderConfig,
    payload::EthPayloadBuilderAttributes,
};
use base_execution_txpool::{BasePooledTransaction, BasePooledTx};
use reth_basic_payload_builder::PayloadConfig;
use reth_evm::execute::BlockBuilder;
use reth_payload_builder::PayloadId;
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::{Account, SealedHeader};
use reth_revm::{database::StateProviderDatabase, db::State, test_utils::StateProviderTest};
use reth_transaction_pool::PoolTransaction;

const fn test_chain_id() -> u64 {
    ChainConfig::mainnet().chain_id
}

fn k1_payer_auth(tx: &TxEip8130, sender: Address, payer: &PrivateKeySigner) -> Bytes {
    let signature = payer.sign_hash_sync(&tx.payer_signature_hash(sender)).unwrap();
    let mut blob = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
    blob.extend_from_slice(&signature.as_bytes());
    Bytes::from(blob)
}

fn signed_8130(
    sender: &PrivateKeySigner,
    payer: Option<&PrivateKeySigner>,
    nonce_key: U256,
    nonce_sequence: u64,
    valid_before: u64,
) -> BasePooledTransaction {
    let payer_address = payer.map(PrivateKeySigner::address);
    let tx = TxEip8130 {
        chain_id: test_chain_id(),
        sender: None,
        nonce_key,
        nonce_sequence,
        valid_after: 0,
        valid_before,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 1_000,
        gas_limit: 1_000_000,
        account_changes: Vec::new(),
        calls: Vec::new(),
        metadata: Bytes::new(),
        payer: payer_address,
    };
    let sender_auth = Bytes::from(
        sender.sign_hash_sync(&tx.sender_signature_hash()).unwrap().as_bytes().to_vec(),
    );
    let payer_auth =
        payer.map_or_else(Bytes::new, |payer| k1_payer_auth(&tx, sender.address(), payer));
    let signed = Eip8130Signed::new(tx, sender_auth, payer_auth);
    let pooled = ConsensusPooledTransaction::Eip8130(signed);
    BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, sender.address()))
}

/// Candidate iterator that mirrors native builder invalidation:
/// protocol-nonce `mark_invalid(sender, nonce)` skips later nonces of that sender;
/// `drop_payer_balance` skips every remaining candidate paid by that payer.
struct InsolventScan {
    queued: VecDeque<BasePooledTransaction>,
    current: Option<BasePooledTransaction>,
    invalid_from: HashMap<Address, u64>,
    unaffordable_payers: HashSet<Address>,
    considered: usize,
}

impl InsolventScan {
    fn new(transactions: Vec<BasePooledTransaction>) -> Self {
        Self {
            queued: transactions.into(),
            current: None,
            invalid_from: HashMap::new(),
            unaffordable_payers: HashSet::new(),
            considered: 0,
        }
    }
}

impl PayloadTransactions for InsolventScan {
    type Transaction = BasePooledTransaction;

    fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
        assert!(self.current.is_none(), "current transaction was not lifecycle-managed");
        while let Some(transaction) = self.queued.pop_front() {
            let sender = transaction.sender();
            let nonce = transaction.nonce();
            if let Some(payer) = CoinbaseTipAffordability::gas_payer(&transaction)
                && self.unaffordable_payers.contains(&payer)
            {
                continue;
            }
            if transaction.eip8130_replay_id().is_none()
                && self.invalid_from.get(&sender).is_some_and(|floor| nonce >= *floor)
            {
                continue;
            }
            self.considered += 1;
            self.current = Some(transaction.clone());
            return Some(transaction);
        }
        None
    }

    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.current = None;
        self.invalid_from
            .entry(sender)
            .and_modify(|floor| *floor = (*floor).min(nonce))
            .or_insert(nonce);
    }
}

impl ParkablePayloadTransactions for InsolventScan {
    fn park_current(&mut self) -> bool {
        false
    }

    fn mark_current_committed(&mut self) {
        self.current = None;
    }

    fn promote(&mut self, _transaction_hash: alloy_primitives::TxHash) -> bool {
        false
    }

    fn discard_parked(&mut self, _transaction_hash: alloy_primitives::TxHash) -> bool {
        false
    }

    fn drop_payer_balance(&mut self, payer: Address, _balance: U256) {
        self.unaffordable_payers.insert(payer);
    }
}

/// Lets the builder take the iterator by value while the test still reads
/// `considered` after `execute_best_transactions` returns.
struct SharedScan {
    inner: Arc<Mutex<InsolventScan>>,
}

impl SharedScan {
    fn new(scan: InsolventScan) -> Self {
        Self { inner: Arc::new(Mutex::new(scan)) }
    }
}

impl PayloadTransactions for SharedScan {
    type Transaction = BasePooledTransaction;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        self.inner.lock().unwrap().next(ctx)
    }

    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.inner.lock().unwrap().mark_invalid(sender, nonce);
    }
}

impl ParkablePayloadTransactions for SharedScan {
    fn park_current(&mut self) -> bool {
        self.inner.lock().unwrap().park_current()
    }

    fn mark_current_committed(&mut self) {
        self.inner.lock().unwrap().mark_current_committed();
    }

    fn promote(&mut self, transaction_hash: alloy_primitives::TxHash) -> bool {
        self.inner.lock().unwrap().promote(transaction_hash)
    }

    fn discard_parked(&mut self, transaction_hash: alloy_primitives::TxHash) -> bool {
        self.inner.lock().unwrap().discard_parked(transaction_hash)
    }

    fn drop_payer_balance(&mut self, payer: Address, balance: U256) {
        self.inner.lock().unwrap().drop_payer_balance(payer, balance);
    }
}

fn payload_context() -> BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec> {
    let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().cobalt_activated().build());
    let parent =
        Arc::new(SealedHeader::seal_slow(Header { gas_limit: 30_000_000, ..Default::default() }));
    let payload_id = PayloadId::new([0; 8]);
    let attributes = BasePayloadBuilderAttributes::<BaseTxEnvelope> {
        payload_attributes: EthPayloadBuilderAttributes {
            id: payload_id,
            parent: parent.hash(),
            timestamp: 2,
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        },
        gas_limit: Some(parent.gas_limit),
        eip_1559_params: Some(B64::ZERO),
        min_base_fee: Some(0),
        ..Default::default()
    };
    BasePayloadBuilderCtx {
        evm_config: BaseEvmConfig::<_, BasePrimitives>::base(Arc::clone(&chain_spec)),
        builder_config: BaseBuilderConfig::default(),
        chain_spec,
        config: PayloadConfig::new(parent, attributes, payload_id),
        cancel: Default::default(),
        best_payload: None,
    }
}

struct CaseResult {
    name: &'static str,
    n: usize,
    build_ms: f64,
    considered: usize,
    included: usize,
}

fn run_build(
    name: &'static str,
    n: usize,
    transactions: Vec<BasePooledTransaction>,
    accounts: &[Address],
) -> CaseResult {
    let scan = SharedScan::new(InsolventScan::new(transactions));
    let considered = Arc::clone(&scan.inner);
    let mut storage = HashMap::default();
    storage.insert(
        StorageKey::from(BaseTime::ADMIN_SLOT.to_be_bytes::<32>()),
        U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()),
    );
    let mut provider = StateProviderTest::default();
    provider.insert_account(
        Predeploys::BASE_TIME,
        Account::default(),
        Some(BaseTime::proxy_bytecode()),
        storage,
    );
    provider.insert_account(
        Predeploys::L1_BLOCK_INFO,
        Account::default(),
        None,
        HashMap::default(),
    );
    for account in accounts {
        provider.insert_account(
            *account,
            Account { balance: U256::ZERO, ..Default::default() },
            None,
            HashMap::default(),
        );
    }

    let ctx = payload_context();
    let mut db = State::builder()
        .with_database(StateProviderDatabase::new(&provider))
        .with_bundle_update()
        .build();
    db.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("load L1 block info");
    let mut builder = ctx.block_builder(&mut db).expect("block builder");
    builder.apply_pre_execution_changes().expect("pre-execution changes");

    let mut info = ExecutionInfo::new();
    let start = Instant::now();
    ctx.execute_best_transactions(&mut info, &mut builder, scan)
        .expect("execute best transactions");
    let build_ms = start.elapsed().as_secs_f64() * 1000.0;

    CaseResult {
        name,
        n,
        build_ms,
        considered: considered.lock().unwrap().considered,
        included: (info.inclusion.standard.txs + info.inclusion.validity.txs) as usize,
    }
}

fn self_pay_protocol(n: usize) -> (Vec<BasePooledTransaction>, Vec<Address>) {
    let signer = PrivateKeySigner::random();
    let txs =
        (0..n as u64).map(|sequence| signed_8130(&signer, None, U256::ZERO, sequence, 0)).collect();
    (txs, vec![signer.address()])
}

fn self_pay_nonce_free(n: usize) -> (Vec<BasePooledTransaction>, Vec<Address>) {
    let signer = PrivateKeySigner::random();
    let txs = (0..n as u64)
        .map(|offset| {
            signed_8130(&signer, None, Eip8130Constants::NONCE_KEY_MAX, 0, 10_000 + offset)
        })
        .collect();
    (txs, vec![signer.address()])
}

fn sponsored_protocol(n: usize) -> (Vec<BasePooledTransaction>, Vec<Address>) {
    let payer = PrivateKeySigner::random();
    let mut accounts = vec![payer.address()];
    let mut txs = Vec::with_capacity(n);
    for _ in 0..n {
        let sender = PrivateKeySigner::random();
        accounts.push(sender.address());
        txs.push(signed_8130(&sender, Some(&payer), U256::ZERO, 0, 0));
    }
    (txs, accounts)
}

/// Models the launch shape at pool scale: many distinct payers, each sponsoring the
/// per-payer cap of transactions across distinct senders. Draining a payer must cost one
/// affordability check that skips that payer's whole group — `considered` should track the
/// number of payers, not the number of transactions, confirming no per-transaction
/// revalidation as the drained backlog scales.
fn capped_sponsors(n: usize) -> (Vec<BasePooledTransaction>, Vec<Address>) {
    const CAP: usize = 4;
    let mut accounts = Vec::new();
    let mut txs = Vec::with_capacity(n);
    let mut payer = PrivateKeySigner::random();
    accounts.push(payer.address());
    for index in 0..n {
        if index > 0 && index % CAP == 0 {
            payer = PrivateKeySigner::random();
            accounts.push(payer.address());
        }
        let sender = PrivateKeySigner::random();
        accounts.push(sender.address());
        txs.push(signed_8130(&sender, Some(&payer), U256::ZERO, 0, 0));
    }
    (txs, accounts)
}

fn bench_sizes() -> Vec<usize> {
    std::env::var("PAYLOAD_BENCH_SIZES")
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|part| part.trim().parse().ok())
                .filter(|size| *size > 0)
                .collect()
        })
        .filter(|sizes: &Vec<usize>| !sizes.is_empty())
        .unwrap_or_else(|| vec![100, 1000])
}

fn print_results(results: &[CaseResult]) {
    eprintln!();
    eprintln!(
        "{:<38} {:>6} {:>12} {:>12} {:>8}",
        "case", "n", "build_ms", "considered", "included"
    );
    eprintln!("{}", "-".repeat(80));
    for result in results {
        eprintln!(
            "{:<38} {:>6} {:>12.3} {:>12} {:>8}",
            result.name, result.n, result.build_ms, result.considered, result.included
        );
    }
    eprintln!();
}

fn measure_all(n: usize) -> Vec<CaseResult> {
    let (txs, accounts) = self_pay_protocol(n);
    let protocol = run_build("1:1 protocol-nonce", n, txs, &accounts);
    let (txs, accounts) = self_pay_nonce_free(n);
    let nonce_free = run_build("1:1 nonce-free", n, txs, &accounts);
    let (txs, accounts) = sponsored_protocol(n);
    let sponsored = run_build("1:Many sponsored protocol-nonce", n, txs, &accounts);
    let (txs, accounts) = capped_sponsors(n);
    let capped = run_build("many capped sponsors (cap=4)", n, txs, &accounts);
    vec![protocol, nonce_free, sponsored, capped]
}

#[test]
fn phase2_insolvent_payload_smoke() {
    let results = measure_all(2);
    for result in &results {
        assert_eq!(result.included, 0, "{} must include none of the drained txs", result.name);
        assert!(
            result.considered >= 1,
            "{} must attempt at least the first insolvent transaction",
            result.name
        );
    }
    assert_eq!(results[0].considered, 1, "protocol-nonce 1:1 must skip the rest of the chain");
    assert_eq!(
        results[1].considered, 1,
        "nonce-free self-pay shares a payer; drop the rest after the first unaffordable check"
    );
    assert_eq!(
        results[2].considered, 1,
        "1:Many senders share a payer; drop the rest after the first unaffordable check"
    );
    assert_eq!(
        results[3].considered, 1,
        "capped sponsors at n=2 share one payer; one drained-payer check skips the group"
    );
}

#[test]
#[ignore = "manual phase-2 timing; run with --ignored --nocapture"]
fn phase2_insolvent_payload_bench() {
    let mut results = Vec::new();
    for n in bench_sizes() {
        results.extend(measure_all(n));
    }
    print_results(&results);
    for result in &results {
        assert_eq!(result.included, 0, "{} included {} drained txs", result.name, result.included);
    }
}
