//! Phase 1: in-process cost of dropping EIP-8130 transactions after a payer drain.
//!
//! Models the launch shape: k1 EOAs with empty code, no keystore, uncapped
//! admission (the high-rate / no-code hypothetical). Two use cases:
//!
//! * **1:1 self-pay** — one account sends and pays for its own transactions
//! * **1:Many sponsorship** — one payer sponsors many other senders
//!
//! Run the measured sizes with:
//! `cargo test -p base-execution-txpool --test high_rate_payer_invalidation \
//!     phase1_invalidation_bench -- --ignored --nocapture`
//!
//! Optional `INVALIDATION_BENCH_SIZES=100,1000` (default `100,1000`).

use std::{sync::Arc, time::Instant};

use alloy_consensus::{
    SignableTransaction, TxEip1559,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::ChainConfig;
use base_common_consensus::{
    BasePooledTransaction as ConsensusPooledTransaction, BasePrimitives, BaseTxEnvelope,
    Eip8130Constants, Eip8130Signed, TxEip8130,
};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::{
    AccountStateDiff, BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, BaseTransactionPool,
    BaseTransactionValidator, GuardLimits,
};
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_tasks::Runtime;
use reth_transaction_pool::{
    Pool, PoolConfig, PoolTransaction, TransactionOrigin, TransactionPool,
    blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
};

type IntegrationPool = BaseTransactionPool<
    MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
    InMemoryBlobStore,
    BaseEvmConfig,
>;

const fn test_chain_id() -> u64 {
    ChainConfig::mainnet().chain_id
}

fn fund(client: &MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>, account: Address) {
    client.add_account(account, ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64)));
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

fn signed_1559(signer: &PrivateKeySigner, nonce: u64) -> BasePooledTransaction {
    let tx = TxEip1559 {
        chain_id: test_chain_id(),
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: 10_000,
        max_priority_fee_per_gas: 10_000,
        to: TxKind::Call(Address::repeat_byte(0xEE)),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
    let recovered = envelope.clone().try_into_recovered().unwrap();
    BasePooledTransaction::new(recovered, envelope.encode_2718_len())
}

fn unlimited_pool() -> (IntegrationPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>) {
    let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().cobalt_activated().build());
    let client = MockEthProvider::<BasePrimitives>::new()
        .with_chain_spec(Arc::clone(&chain_spec))
        .with_genesis_block();
    let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
    let blob_store = InMemoryBlobStore::default();
    let validator = EthTransactionValidatorBuilder::new(client.clone(), evm_config)
        .no_shanghai()
        .no_cancun()
        .build_with_tasks(Runtime::test(), blob_store.clone())
        .map(|inner| {
            BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default())
                .require_l1_data_gas_fee(false)
        });
    let ordering = BaseOrdering::default();
    let pool = Pool::new(
        validator,
        ordering.clone(),
        blob_store,
        PoolConfig {
            max_account_slots: 16_384,
            max_inflight_delegated_slot_limit: 16_384,
            ..Default::default()
        },
    );
    let pool = BaseTransactionPool::new(pool, ordering)
        .with_guard_limits(GuardLimits { signature_limit: u32::MAX, payment_limit: u32::MAX });
    (pool, client)
}

fn bench_sizes() -> Vec<usize> {
    std::env::var("INVALIDATION_BENCH_SIZES")
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

struct CaseResult {
    name: &'static str,
    n: usize,
    admit_ms: f64,
    drop_ms: f64,
    dropped: usize,
}

async fn admit_self_pay_protocol(
    pool: &IntegrationPool,
    client: &MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
    n: usize,
) -> Address {
    let signer = PrivateKeySigner::random();
    fund(client, signer.address());
    for sequence in 0..n as u64 {
        let tx = signed_8130(&signer, None, U256::ZERO, sequence, 0);
        pool.add_transaction(TransactionOrigin::Local, tx).await.expect("self-pay protocol nonce");
    }
    signer.address()
}

async fn admit_self_pay_nonce_free(
    pool: &IntegrationPool,
    client: &MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
    n: usize,
) -> Address {
    let signer = PrivateKeySigner::random();
    fund(client, signer.address());
    for offset in 0..n as u64 {
        let tx = signed_8130(&signer, None, Eip8130Constants::NONCE_KEY_MAX, 0, offset + 1);
        pool.add_transaction(TransactionOrigin::Local, tx).await.expect("self-pay nonce-free");
    }
    signer.address()
}

async fn admit_sponsored_protocol(
    pool: &IntegrationPool,
    client: &MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
    n: usize,
) -> Address {
    let payer = PrivateKeySigner::random();
    fund(client, payer.address());
    for _ in 0..n {
        let sender = PrivateKeySigner::random();
        fund(client, sender.address());
        let tx = signed_8130(&sender, Some(&payer), U256::ZERO, 0, 0);
        pool.add_transaction(TransactionOrigin::Local, tx).await.expect("sponsored protocol nonce");
    }
    payer.address()
}

async fn measure_self_pay_protocol(n: usize) -> CaseResult {
    let (pool, client) = unlimited_pool();
    let admit_start = Instant::now();
    let payer = admit_self_pay_protocol(&pool, &client, n).await;
    let admit_ms = admit_start.elapsed().as_secs_f64() * 1000.0;
    drop_payer(&pool, "1:1 protocol-nonce", n, payer, admit_ms)
}

async fn measure_self_pay_nonce_free(n: usize) -> CaseResult {
    let (pool, client) = unlimited_pool();
    let admit_start = Instant::now();
    let payer = admit_self_pay_nonce_free(&pool, &client, n).await;
    let admit_ms = admit_start.elapsed().as_secs_f64() * 1000.0;
    drop_payer(&pool, "1:1 nonce-free", n, payer, admit_ms)
}

async fn measure_sponsored_protocol(n: usize) -> CaseResult {
    let (pool, client) = unlimited_pool();
    let admit_start = Instant::now();
    let payer = admit_sponsored_protocol(&pool, &client, n).await;
    let admit_ms = admit_start.elapsed().as_secs_f64() * 1000.0;
    drop_payer(&pool, "1:Many sponsored protocol-nonce", n, payer, admit_ms)
}

async fn measure_self_pay_protocol_with_1559_drain(n: usize) -> CaseResult {
    let (pool, client) = unlimited_pool();
    let admit_start = Instant::now();
    let signer = PrivateKeySigner::random();
    fund(&client, signer.address());
    for sequence in 0..n as u64 {
        let tx = signed_8130(&signer, None, U256::ZERO, sequence, 0);
        pool.add_transaction(TransactionOrigin::Local, tx).await.expect("self-pay protocol nonce");
    }
    let drain = signed_1559(&signer, n as u64);
    pool.add_transaction(TransactionOrigin::Local, drain).await.expect("1559 drain sibling");
    let admit_ms = admit_start.elapsed().as_secs_f64() * 1000.0;
    drop_payer(&pool, "1:1 protocol-nonce + pooled 1559", n, signer.address(), admit_ms)
}

fn drop_payer(
    pool: &IntegrationPool,
    name: &'static str,
    n: usize,
    payer: Address,
    admit_ms: f64,
) -> CaseResult {
    let drop_start = Instant::now();
    let removed = pool.apply_state_diff(&[AccountStateDiff {
        address: payer,
        balance: Some(U256::ZERO),
        nonce_changed: false,
        code_changed: false,
        changed_slots: Vec::new(),
    }]);
    let drop_ms = drop_start.elapsed().as_secs_f64() * 1000.0;
    CaseResult { name, n, admit_ms, drop_ms, dropped: removed.len() }
}

fn print_results(results: &[CaseResult]) {
    eprintln!();
    eprintln!("{:<38} {:>6} {:>12} {:>12} {:>8}", "case", "n", "admit_ms", "drop_ms", "dropped");
    eprintln!("{}", "-".repeat(80));
    for result in results {
        eprintln!(
            "{:<38} {:>6} {:>12.3} {:>12.3} {:>8}",
            result.name, result.n, result.admit_ms, result.drop_ms, result.dropped
        );
    }
    eprintln!();
}

#[tokio::test]
async fn phase1_invalidation_smoke() {
    for n in [2_usize] {
        let protocol = measure_self_pay_protocol(n).await;
        assert_eq!(protocol.dropped, n, "self-pay protocol-nonce must drop every 8130");
        let nonce_free = measure_self_pay_nonce_free(n).await;
        assert_eq!(nonce_free.dropped, n, "self-pay nonce-free must drop every 8130");
        let sponsored = measure_sponsored_protocol(n).await;
        assert_eq!(sponsored.dropped, n, "sponsored protocol-nonce must drop every 8130");
        let with_1559 = measure_self_pay_protocol_with_1559_drain(n).await;
        assert_eq!(
            with_1559.dropped, n,
            "pooled 1559 must not be deleted by the 8130 guard; only the 8130s drop"
        );
    }
}

#[tokio::test]
#[ignore = "manual phase-1 timing; run with --ignored --nocapture"]
async fn phase1_invalidation_bench() {
    let mut results = Vec::new();
    for n in bench_sizes() {
        results.push(measure_self_pay_protocol(n).await);
        results.push(measure_self_pay_nonce_free(n).await);
        results.push(measure_sponsored_protocol(n).await);
        results.push(measure_self_pay_protocol_with_1559_drain(n).await);
    }
    print_results(&results);
    for result in &results {
        assert_eq!(
            result.dropped, result.n,
            "{} n={} dropped {} 8130s",
            result.name, result.n, result.dropped
        );
    }
}
