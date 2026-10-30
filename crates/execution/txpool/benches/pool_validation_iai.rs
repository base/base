//! Deterministic instruction-count benchmark for transaction pool admission validation of
//! user-submitted transactions.
//!
//! Every `eth_sendRawTransaction` call (~10^8 submissions/day) decodes and recovers the raw
//! transaction and has the Base validator check it before the pool admits it. The counted region
//! runs that work for a fixed batch of [`BATCH_SIZE`] signed EIP-1559 transactions of median
//! mainnet size (about 460 bytes encoded, ABI-encoded calldata), one transaction at a time as
//! the RPC handler submits them:
//!
//! - `PoolTransaction::recover_raw_transaction`, the hook reth's `send_raw_transaction` calls
//!   when no sender recovery cache is configured: EIP-2718 decoding and signature recovery.
//! - `TransactionValidator::validate_transaction` on the production [`BaseTransactionValidator`]
//!   with a local origin: reth's stateless and stateful checks against a fresh state provider,
//!   the DA-footprint check, and the L1 data fee plus operator fee from the FastLZ-estimated
//!   size, checked against the sender's balance.
//!
//! The validator reads an in-memory state provider, so state reads are cheaper than against a
//! node's database and their share is a lower bound. The pool insert that follows validation,
//! and the hop through `TransactionValidationTaskExecutor`, are not counted; `builder_insert_iai`
//! counts both. No metrics recorder is installed, so metric updates cost only their no-op path.
//! L1 block info comes from an Isthmus L1-attributes deposit, so the L1 data fee and operator
//! fee are non-zero.
//!
//! The `valid` case submits only valid transactions. In `mixed`, one in [`INVALID_EVERY`]
//! reuses a nonce below the sender's account nonce, and as many come from senders funded for the
//! gas and value but not the L1 fee, so they pass reth's cost check and fail only Base's.
//!
//! Signer keys and calldata are derived from their index, so the batch, and with it every hash
//! and map layout, is identical across runs. Building the validator, funding the senders,
//! signing and encoding the batch, and one warm-up validation that pays one-time lazy
//! initialization (such as the signature recovery context) run in iai's unmeasured setup, and the
//! inputs are handed back so they drop outside the counted region.
//!
//! The teardown pins how many transactions validated, failed the nonce check, and failed the
//! balance check. They are fixed by the workload, so a mismatch means admission now accepts
//! different transactions, and the instruction count is not comparable with the base commit's.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::{hint::black_box, sync::Arc};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, hex, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::ChainConfig;
use base_common_consensus::{BasePrimitives, BaseTransactionSigned, TxDeposit};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::{BaseL1BlockInfo, BasePooledTransaction, BaseTransactionValidator};
use base_test_utils::build_test_genesis_everest;
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_transaction_pool::{
    PoolTransaction, TransactionOrigin, TransactionValidationOutcome, TransactionValidator,
    blobstore::InMemoryBlobStore, error::InvalidPoolTransactionError,
    validate::EthTransactionValidatorBuilder,
};
use tokio::runtime::Runtime;

type Provider = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
type Validator = BaseTransactionValidator<Provider, BasePooledTransaction, BaseEvmConfig>;
type Outcomes = Vec<TransactionValidationOutcome<BasePooledTransaction>>;

/// Transactions validated per run.
const BATCH_SIZE: u64 = 64;

/// In the `mixed` case, one in this many transactions has a stale nonce and one in this many
/// cannot pay its L1 fee.
const INVALID_EVERY: u64 = 8;

/// ABI words of calldata after the selector, sized so the encoded transaction is about the
/// median mainnet size.
const CALLDATA_WORDS: u64 = 11;

/// Wall clock the validator starts at, in Unix seconds.
const NOW_SECS: u64 = 1_700_000_000;

/// L1-attributes deposit calldata that activates Isthmus with non-zero fee scalars, operator fee
/// scalar and operator fee constant. The fixture of `parse_l1_info_isthmus` in
/// `crates/execution/evm/src/l1.rs`.
const ISTHMUS_L1_INFO_DATA_HEX: &str = concat!(
    "098999be00000558000c5fc500000000000000030000000067a9f765",
    "0000000000000029000000000000000000000000000000000000000000000000",
    "00000000006a6d090000000000000000000000000000000000000000000000000000000000000001",
    "72fcc8e8886636bdbe96ba0e4baab67ea7e7811633f52b52e8cf7a5123213b6f",
    "000000000000000000000000d3f2c5afb2d76f5579f326b0cd7da5f5a4126c35",
    "00004e2000000000000001f4",
);

/// What a batch transaction is built to do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Passes every check.
    Valid,
    /// Reuses a nonce below the sender's account nonce.
    StaleNonce,
    /// Sender holds the gas and value but not the L1 fee.
    Underfunded,
}

/// Outcome counts of one batch.
#[derive(Debug, Default, PartialEq, Eq)]
struct Totals {
    valid: usize,
    stale_nonce: usize,
    insufficient_funds: usize,
}

/// A validator ready to admit one batch of raw transactions.
struct Input {
    runtime: Runtime,
    validator: Validator,
    raw: Vec<Bytes>,
    mixed: bool,
}

/// The production Base validator over an in-memory state provider, with Isthmus L1 block info.
fn build_validator() -> (Validator, Provider) {
    let mut genesis = build_test_genesis_everest();
    genesis.config.chain_id = ChainConfig::mainnet().chain_id;
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
    let client = MockEthProvider::<BasePrimitives>::new()
        .with_chain_spec(Arc::clone(&chain_spec))
        .with_genesis_block();
    let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
    let inner = EthTransactionValidatorBuilder::new(client.clone(), evm_config)
        .no_shanghai()
        .no_cancun()
        .build(InMemoryBlobStore::default());
    let validator = BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default());
    let l1_info: BaseTransactionSigned = TxDeposit {
        source_hash: B256::ZERO,
        from: Address::ZERO,
        to: TxKind::Create,
        mint: 0,
        value: U256::ZERO,
        gas_limit: 0,
        is_system_transaction: false,
        input: hex::decode(ISTHMUS_L1_INFO_DATA_HEX).expect("valid hex fixture").into(),
    }
    .into();
    let header = alloy_consensus::Header { timestamp: NOW_SECS, ..Default::default() };
    validator.update_l1_block_info(&header, &[l1_info]);
    (validator, client)
}

/// ABI-encoded calldata for transaction `index`: a selector, then address, amount and hash
/// words, mixing the zero padding and high-entropy bytes real calls carry.
fn calldata(index: u64) -> Bytes {
    let mut data = vec![0x41, 0x4b, 0xf3, 0x89];
    for word in 0..CALLDATA_WORDS {
        let seed = keccak256([index.to_be_bytes(), word.to_be_bytes()].concat());
        match word % 3 {
            0 => data.extend([[0; 12].as_slice(), &seed[..20]].concat()),
            1 => data.extend([[0; 24].as_slice(), &seed[..8]].concat()),
            _ => data.extend(seed),
        }
    }
    data.into()
}

/// The EIP-2718 bytes of transaction `index` of the batch, after funding its sender in `client`
/// so the transaction ends up as `kind`.
fn raw_transaction(client: &Provider, index: u64, kind: Kind) -> Bytes {
    let signer = PrivateKeySigner::from_bytes(&B256::from(U256::from(index + 1))).expect("key");
    let account_nonce = if kind == Kind::StaleNonce { 5 } else { index % 4 };
    let tx = TxEip1559 {
        chain_id: ChainConfig::mainnet().chain_id,
        nonce: if kind == Kind::StaleNonce { account_nonce - 1 } else { account_nonce },
        gas_limit: 200_000,
        max_fee_per_gas: 10_000_000,
        max_priority_fee_per_gas: 1_000_000,
        to: TxKind::Call(Address::repeat_byte(0x42)),
        value: U256::from(index * 1_000),
        access_list: Default::default(),
        input: calldata(index),
    };
    let balance = if kind == Kind::Underfunded {
        tx.value + U256::from(u128::from(tx.gas_limit) * tx.max_fee_per_gas)
    } else {
        U256::from(1_000_000_000_000_000_000u64)
    };
    client.add_account(signer.address(), ExtendedAccount::new(account_nonce, balance));
    let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("signs");
    let signed: BaseTransactionSigned = tx.into_signed(signature).into();
    signed.encoded_2718().into()
}

/// What transaction `index` of the batch is built to do.
const fn kind(mixed: bool, index: u64) -> Kind {
    match index % INVALID_EVERY {
        3 if mixed => Kind::StaleNonce,
        6 if mixed => Kind::Underfunded,
        _ => Kind::Valid,
    }
}

/// Decodes, recovers and validates `raw` as `eth_sendRawTransaction` submits it.
async fn admit(
    validator: &Validator,
    raw: &Bytes,
) -> TransactionValidationOutcome<BasePooledTransaction> {
    let transaction = BasePooledTransaction::recover_raw_transaction(raw).expect("recovers");
    validator.validate_transaction(TransactionOrigin::Local, transaction).await
}

/// Builds the validator and signs the batch outside the counted region.
fn setup(mixed: bool) -> Input {
    let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
    let (validator, client) = build_validator();

    // Index `BATCH_SIZE` is the warm-up sender; the batch uses indices below it.
    let warm_up = raw_transaction(&client, BATCH_SIZE, Kind::Valid);
    let outcome = runtime.block_on(admit(&validator, &warm_up));
    assert!(outcome.is_valid(), "warm-up transaction must validate: {outcome:?}");

    let raw: Vec<Bytes> =
        (0..BATCH_SIZE).map(|index| raw_transaction(&client, index, kind(mixed, index))).collect();
    assert!(
        raw.iter().all(|raw| (450..=480).contains(&raw.len())),
        "transactions must be about the median mainnet size: {:?}",
        raw.iter().map(|raw| raw.len()).collect::<Vec<_>>()
    );
    Input { runtime, validator, raw, mixed }
}

/// Checks the batch's outcomes against the workload's documented counts.
fn check_outcomes((outcomes, input): (Outcomes, Input)) {
    let mut totals = Totals::default();
    for outcome in &outcomes {
        match outcome {
            TransactionValidationOutcome::Valid { .. } => totals.valid += 1,
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::Consensus(
                    InvalidTransactionError::NonceNotConsistent { .. },
                ),
            ) => totals.stale_nonce += 1,
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::Consensus(InvalidTransactionError::InsufficientFunds(
                    _,
                )),
            ) => totals.insufficient_funds += 1,
            other => panic!("unexpected validation outcome: {other:?}"),
        }
    }
    let expected = if input.mixed {
        Totals { valid: 48, stale_nonce: 8, insufficient_funds: 8 }
    } else {
        Totals { valid: 64, stale_nonce: 0, insufficient_funds: 0 }
    };
    assert_eq!(totals, expected, "pool_validation mixed={} admission changed", input.mixed);
}

#[library_benchmark]
#[bench::valid(args = (false), setup = setup, teardown = check_outcomes)]
#[bench::mixed(args = (true), setup = setup, teardown = check_outcomes)]
fn validate_batch(input: Input) -> (Outcomes, Input) {
    let outcomes = input.runtime.block_on(async {
        let mut outcomes = Vec::with_capacity(input.raw.len());
        for raw in &input.raw {
            outcomes.push(admit(&input.validator, raw).await);
        }
        outcomes
    });
    (black_box(outcomes), input)
}

library_benchmark_group!(
    name = pool_validation;
    benchmarks = validate_batch
);

main!(library_benchmark_groups = pool_validation);
