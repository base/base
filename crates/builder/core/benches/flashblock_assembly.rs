//! Benchmarks cumulative payload and incremental flashblock assembly.
//!
//! Each case holds the current flashblock delta at ten transactions while increasing the
//! cumulative transaction prefix. This exposes work that scales with the full block rather than
//! with newly included transactions.

use std::{hint::black_box, sync::Arc};

use alloy_consensus::{Header, Receipt, TxEip1559};
use alloy_primitives::{TxKind, U256};
use base_builder_core::{
    BasePayloadBuilderCtx, ExecutionInfo, FlashblockAssembler, StateRootMode,
    test_utils::{generate_signer_from_seed, sign_base_tx},
};
use base_common_consensus::{BaseReceipt, BaseTypedTransaction};
use base_common_flashblocks::FlashblockId;
use base_execution_chainspec::BaseChainSpec;
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use reth_chainspec::ChainSpec;
use reth_primitives_traits::SealedHeader;
use reth_provider::noop::NoopProvider;
use reth_revm::{State, database::StateProviderDatabase};

const DELTA_TRANSACTIONS: usize = 10;
const CUMULATIVE_PREFIXES: &[usize] = &[0, 100, 500, 1_000];

fn minimal_chain_spec() -> Arc<BaseChainSpec> {
    let genesis: serde_json::Value = serde_json::json!({
        "config": { "chainId": 901 },
        "gasLimit": "0x1C9C380",
        "timestamp": "0x0"
    });
    let genesis = serde_json::from_value(genesis).expect("valid genesis");
    let inner = ChainSpec::builder().chain(901.into()).genesis(genesis).cancun_activated().build();
    Arc::new(BaseChainSpec::from(inner))
}

fn context() -> BasePayloadBuilderCtx {
    let parent = Arc::new(SealedHeader::seal_slow(Header {
        gas_limit: 30_000_000,
        timestamp: 0,
        ..Default::default()
    }));
    BasePayloadBuilderCtx::for_test(minimal_chain_spec(), parent)
}

fn execution_info(transaction_count: usize) -> ExecutionInfo {
    let signer = generate_signer_from_seed("flashblock-assembly-benchmark");
    let mut info = ExecutionInfo::with_capacity(transaction_count);

    for nonce in 0..transaction_count {
        let tx = TxEip1559 {
            chain_id: 901,
            nonce: nonce as u64,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(signer.address()),
            value: U256::from(1u64),
            ..Default::default()
        };
        let signed = sign_base_tx(&signer, BaseTypedTransaction::Eip1559(tx))
            .expect("sign benchmark transaction")
            .into_inner();
        let cumulative_gas_used = (nonce as u64 + 1) * 21_000;

        info.executed_transactions.push(signed);
        info.executed_senders.push(signer.address());
        info.receipts.push(BaseReceipt::Eip1559(Receipt {
            status: true.into(),
            cumulative_gas_used,
            logs: Vec::new(),
        }));
        info.cumulative_gas_used = cumulative_gas_used;
    }

    info
}

fn state() -> State<StateProviderDatabase<NoopProvider>> {
    State::builder()
        .with_database(StateProviderDatabase::new(NoopProvider::default()))
        .with_bundle_update()
        .build()
}

fn assembly_benches(c: &mut Criterion) {
    let ctx = context();
    let all_transactions = execution_info(
        CUMULATIVE_PREFIXES.iter().copied().max().unwrap_or_default() + DELTA_TRANSACTIONS,
    );
    let mut group = c.benchmark_group("flashblock_assembly/fixed_delta");

    for &prefix in CUMULATIVE_PREFIXES {
        let mut info = ExecutionInfo {
            executed_transactions: all_transactions.executed_transactions[..prefix].to_vec(),
            executed_senders: all_transactions.executed_senders[..prefix].to_vec(),
            receipts: all_transactions.receipts[..prefix].to_vec(),
            cumulative_gas_used: prefix as u64 * 21_000,
            ..ExecutionInfo::default()
        };

        if prefix > 0 {
            FlashblockAssembler::build::<_, NoopProvider>(
                &mut state(),
                &ctx,
                &mut info,
                FlashblockId::default(),
                StateRootMode::Skip,
            )
            .expect("prefix assembly should succeed");
        }

        let end = prefix + DELTA_TRANSACTIONS;
        info.executed_transactions
            .extend_from_slice(&all_transactions.executed_transactions[prefix..end]);
        info.executed_senders.extend_from_slice(&all_transactions.executed_senders[prefix..end]);
        info.receipts.extend_from_slice(&all_transactions.receipts[prefix..end]);
        info.cumulative_gas_used = end as u64 * 21_000;

        group.bench_with_input(BenchmarkId::from_parameter(prefix), &info, |b, info| {
            b.iter_batched(
                || (state(), info.clone()),
                |(mut state, mut info)| {
                    let assembly = FlashblockAssembler::build::<_, NoopProvider>(
                        &mut state,
                        &ctx,
                        &mut info,
                        FlashblockId::default(),
                        StateRootMode::Skip,
                    )
                    .expect("assembly should succeed");
                    black_box(assembly.flashblock.diff.block_hash)
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, assembly_benches);
criterion_main!(benches);
