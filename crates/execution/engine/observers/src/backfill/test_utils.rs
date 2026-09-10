use std::sync::Arc;

use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, TxKind, U256, b256};
use base_common_chain_config::{BaseChainSpec, BaseChainSpecBuilder};
use base_common_types_chain::RecoveredBlock;
use base_common_types_chain::{
    BaseBlock, BaseBlockBody, BaseTypedTransaction, BlockHeader, Header, TxEip2930,
    constants::ETH_TO_WEI,
};
use base_execution_evm_blocks::{BaseEvmConfig, BlockExecutionOutput, Executor};
use base_execution_state_provider::{
    BlockWriter as _, ExecutionOutcome, LatestStateProvider, ProviderFactory,
};
use secp256k1::Keypair;

pub(crate) fn to_execution_outcome(
    block_number: u64,
    block_execution_output: &BlockExecutionOutput,
) -> ExecutionOutcome {
    ExecutionOutcome {
        bundle: block_execution_output.state.clone(),
        receipts: vec![block_execution_output.receipts.clone()],
        first_block: block_number,
        requests: vec![block_execution_output.requests.clone()],
    }
}

pub(crate) fn chain_spec(address: Address) -> Arc<BaseChainSpec> {
    // Create a chain spec with a genesis state that contains the
    // provided sender
    Arc::new(
        BaseChainSpecBuilder::default()
            .chain(std::sync::Arc::new(base_common_chain_config::BaseChainSpec::mainnet()).chain())
            .genesis(Genesis {
                alloc: [(
                    address,
                    GenesisAccount { balance: U256::from(ETH_TO_WEI), ..Default::default() },
                )]
                .into(),
                ..std::sync::Arc::new(base_common_chain_config::BaseChainSpec::mainnet())
                    .genesis
                    .clone()
            })
            .bedrock_activated()
            .build(),
    )
}

pub(crate) fn execute_block_and_commit_to_database(
    provider_factory: &ProviderFactory,
    chain_spec: Arc<BaseChainSpec>,
    block: &RecoveredBlock,
) -> eyre::Result<BlockExecutionOutput> {
    let provider = provider_factory.provider()?;

    // Execute the block to produce a block execution output
    let mut block_execution_output = BaseEvmConfig::new(chain_spec)
        .batch_executor(LatestStateProvider::new(provider))
        .execute(block)?;
    block_execution_output.state.reverts.sort();

    // Convert the block execution output to an execution outcome for committing to the database
    let execution_outcome = to_execution_outcome(block.number(), &block_execution_output);

    // Commit the block's execution outcome to the database
    let hashed_state = execution_outcome.hash_state_slow().into_sorted();
    let provider_rw = provider_factory.provider_rw()?;
    provider_rw.append_blocks_with_state(vec![block.clone()], &execution_outcome, hashed_state)?;
    provider_rw.commit()?;

    Ok(block_execution_output)
}

fn blocks(
    chain_spec: Arc<BaseChainSpec>,
    key_pair: Keypair,
) -> eyre::Result<(RecoveredBlock, RecoveredBlock)> {
    // First block has a transaction that transfers some ETH to zero address
    let block1 = BaseBlock {
        header: Header {
            parent_hash: chain_spec.genesis_hash(),
            receipts_root: b256!(
                "0xd3a6acf9a244d78b33831df95d472c4128ea85bf079a1d41e32ed0b7d2244c9e"
            ),
            number: 1,
            gas_limit: 21_000u64,
            gas_used: 21_000u64,
            ..Default::default()
        },
        body: BaseBlockBody {
            transactions: vec![base_testing_support::BaseTestData::sign_tx_with_key_pair(
                key_pair,
                BaseTypedTransaction::Eip2930(TxEip2930 {
                    chain_id: chain_spec.chain().id(),
                    nonce: 0,
                    gas_limit: 21_000u64,
                    gas_price: 1_500_000_000,
                    to: TxKind::Call(Address::ZERO),
                    value: U256::from(0.1 * ETH_TO_WEI as f64),
                    ..Default::default()
                }),
            )],
            ..Default::default()
        },
    }
    .try_into_recovered()?;

    // Second block resends the same transaction with increased nonce
    let block2 = BaseBlock {
        header: Header {
            parent_hash: block1.hash(),
            receipts_root: b256!(
                "0xd3a6acf9a244d78b33831df95d472c4128ea85bf079a1d41e32ed0b7d2244c9e"
            ),
            number: 2,
            gas_limit: 21_000u64,
            gas_used: 21_000u64,
            ..Default::default()
        },
        body: BaseBlockBody {
            transactions: vec![base_testing_support::BaseTestData::sign_tx_with_key_pair(
                key_pair,
                BaseTypedTransaction::Eip2930(TxEip2930 {
                    chain_id: chain_spec.chain().id(),
                    nonce: 1,
                    gas_limit: 21_000u64,
                    gas_price: 1_500_000_000,
                    to: TxKind::Call(Address::ZERO),
                    value: U256::from(0.1 * ETH_TO_WEI as f64),
                    ..Default::default()
                }),
            )],
            ..Default::default()
        },
    }
    .try_into_recovered()?;

    Ok((block1, block2))
}

pub(crate) fn blocks_and_execution_outputs(
    provider_factory: ProviderFactory,
    chain_spec: Arc<BaseChainSpec>,
    key_pair: Keypair,
) -> eyre::Result<Vec<(RecoveredBlock, BlockExecutionOutput)>> {
    let (block1, block2) = blocks(chain_spec.clone(), key_pair)?;

    let block_output1 =
        execute_block_and_commit_to_database(&provider_factory, chain_spec.clone(), &block1)?;
    let block_output2 =
        execute_block_and_commit_to_database(&provider_factory, chain_spec, &block2)?;

    Ok(vec![(block1, block_output1), (block2, block_output2)])
}

pub(crate) fn blocks_and_execution_outcome(
    provider_factory: ProviderFactory,
    chain_spec: Arc<BaseChainSpec>,
    key_pair: Keypair,
) -> eyre::Result<(Vec<RecoveredBlock>, ExecutionOutcome)> {
    let (block1, block2) = blocks(chain_spec.clone(), key_pair)?;

    let provider = provider_factory.provider()?;

    let evm_config = BaseEvmConfig::new(chain_spec);
    let executor = evm_config.batch_executor(LatestStateProvider::new(provider));

    let mut execution_outcome = executor.execute_batch(vec![&block1, &block2])?;
    execution_outcome.state_mut().reverts.sort();

    // Commit the block's execution outcome to the database
    let hashed_state = execution_outcome.hash_state_slow().into_sorted();
    let provider_rw = provider_factory.provider_rw()?;
    provider_rw.append_blocks_with_state(
        vec![block1.clone(), block2.clone()],
        &execution_outcome,
        hashed_state,
    )?;
    provider_rw.commit()?;

    Ok((vec![block1, block2], execution_outcome))
}
