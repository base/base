use std::sync::Arc;

use base_common_consensus::BaseTxEnvelope;
use reth_consensus::{FullConsensus, noop::NoopConsensus};
use reth_db::DatabaseEnv;
use reth_db_api::{
    cursor::DbCursorRO, database::Database, table::TableImporter, tables, transaction::DbTx,
};
use reth_db_common::DbTool;
use reth_evm::BaseEvmConfig;
use reth_node_api::NodeTypesWithDB;
use reth_node_core::dirs::{ChainPath, DataDirPath};
use reth_provider::{
    DatabaseProviderFactory, ProviderFactory,
    providers::{RocksDBProvider, StaticFileProvider},
};
use reth_stages::{Stage, StageCheckpoint, UnwindInput, stages::ExecutionStage};
use tracing::info;

use super::setup;

#[expect(clippy::too_many_arguments)]
pub(crate) async fn dump_execution_stage<N, C>(
    db_tool: &DbTool<N>,
    from: u64,
    to: u64,
    output_datadir: ChainPath<DataDirPath>,
    should_run: bool,
    evm_config: BaseEvmConfig,
    consensus: C,
    runtime: reth_tasks::Runtime,
) -> eyre::Result<()>
where
    N: NodeTypesWithDB<DB = DatabaseEnv>,
    C: FullConsensus + 'static,
{
    let (output_db, tip_block_number) = setup(from, to, &output_datadir.db(), db_tool)?;

    import_tables_with_range(&output_db, db_tool, from, to)?;

    unwind_and_copy(db_tool, from, tip_block_number, &output_db, evm_config.clone())?;

    if should_run {
        dry_run(
            ProviderFactory::<N>::new(
                output_db,
                db_tool.chain(),
                StaticFileProvider::read_write(output_datadir.static_files())?,
                RocksDBProvider::builder(output_datadir.rocksdb()).build()?,
                runtime,
            )?,
            to,
            from,
            evm_config,
            consensus,
        )?;
    }

    Ok(())
}

/// Imports all the tables that can be copied over a range.
fn import_tables_with_range<N: NodeTypesWithDB>(
    output_db: &DatabaseEnv,
    db_tool: &DbTool<N>,
    from: u64,
    to: u64,
) -> eyre::Result<()> {
    //  We're not sharing the transaction in case the memory grows too much.

    output_db.update(|tx| {
        tx.import_table_with_range::<tables::CanonicalHeaders, _>(
            &db_tool.provider_factory.db_ref().tx()?,
            Some(from),
            to,
        )
    })??;
    output_db.update(|tx| {
        tx.import_table_with_range::<tables::Headers<alloy_consensus::Header>, _>(
            &db_tool.provider_factory.db_ref().tx()?,
            Some(from),
            to,
        )
    })??;
    output_db.update(|tx| {
        tx.import_table_with_range::<tables::BlockBodyIndices, _>(
            &db_tool.provider_factory.db_ref().tx()?,
            Some(from),
            to,
        )
    })??;
    output_db.update(|tx| {
        tx.import_table_with_range::<tables::BlockOmmers<alloy_consensus::Header>, _>(
            &db_tool.provider_factory.db_ref().tx()?,
            Some(from),
            to,
        )
    })??;

    // Find range of transactions that need to be copied over
    let (from_tx, to_tx) = db_tool.provider_factory.db_ref().view(|read_tx| {
        let mut read_cursor = read_tx.cursor_read::<tables::BlockBodyIndices>()?;
        let (_, from_block) =
            read_cursor.seek(from)?.ok_or(eyre::eyre!("BlockBody {from} does not exist."))?;
        let (_, to_block) =
            read_cursor.seek(to)?.ok_or(eyre::eyre!("BlockBody {to} does not exist."))?;

        Ok::<(u64, u64), eyre::ErrReport>((
            from_block.first_tx_num,
            to_block.first_tx_num + to_block.tx_count,
        ))
    })??;

    output_db.update(|tx| {
        tx.import_table_with_range::<tables::Transactions<BaseTxEnvelope>, _>(
            &db_tool.provider_factory.db_ref().tx()?,
            Some(from_tx),
            to_tx,
        )
    })??;

    output_db.update(|tx| {
        tx.import_table_with_range::<tables::TransactionSenders, _>(
            &db_tool.provider_factory.db_ref().tx()?,
            Some(from_tx),
            to_tx,
        )
    })??;

    Ok(())
}

/// Dry-run an unwind to FROM block, so we can get the `PlainStorageState` and
/// `PlainAccountState` safely. There might be some state dependency from an address
/// which hasn't been changed in the given range.
fn unwind_and_copy<N: NodeTypesWithDB>(
    db_tool: &DbTool<N>,
    from: u64,
    tip_block_number: u64,
    output_db: &DatabaseEnv,
    evm_config: BaseEvmConfig,
) -> eyre::Result<()> {
    let provider = db_tool.provider_factory.database_provider_rw()?;

    let mut exec_stage = ExecutionStage::new_with_executor(evm_config, NoopConsensus::arc());

    exec_stage.unwind(
        &provider,
        UnwindInput {
            unwind_to: from,
            checkpoint: StageCheckpoint::new(tip_block_number),
            bad_block: None,
        },
    )?;

    let unwind_inner_tx = provider.into_tx();

    output_db
        .update(|tx| tx.import_dupsort::<tables::PlainStorageState, _>(&unwind_inner_tx))??;
    output_db.update(|tx| tx.import_table::<tables::PlainAccountState, _>(&unwind_inner_tx))??;
    output_db.update(|tx| tx.import_table::<tables::Bytecodes, _>(&unwind_inner_tx))??;

    Ok(())
}

/// Try to re-execute the stage without committing
fn dry_run<N, C>(
    output_provider_factory: ProviderFactory<N>,
    to: u64,
    from: u64,
    evm_config: BaseEvmConfig,
    consensus: C,
) -> eyre::Result<()>
where
    N: NodeTypesWithDB,
    C: FullConsensus + 'static,
{
    info!(target: "reth::cli", "Executing stage. [dry-run]");

    let mut exec_stage = ExecutionStage::new_with_executor(evm_config, Arc::new(consensus));

    let input =
        reth_stages::ExecInput { target: Some(to), checkpoint: Some(StageCheckpoint::new(from)) };
    exec_stage.execute(&output_provider_factory.database_provider_rw()?, input)?;

    info!(target: "reth::cli", "Success");

    Ok(())
}
