//! Command that initializes the node from a genesis file.

use std::{io::BufReader, path::PathBuf, sync::Arc};

use crate::ChainSpecParser;
use alloy_primitives::B256;
use base_common_types_chain::BlockHeader as AlloyBlockHeader;
use base_execution_chainspec::BaseChainSpec;
use clap::Parser;
use reth_db_common::init::init_from_state_dump;
use reth_primitives_traits::{SealedHeader, header::HeaderMut};
use reth_provider::{
    BlockNumReader, DBProvider, DatabaseProviderFactory, StaticFileProviderFactory,
    StaticFileWriter,
};
use tracing::info;

use crate::common::{AccessRights, Environment, EnvironmentArgs};

pub mod without_evm;

/// Initializes the database with the genesis block.
#[derive(Debug, Parser)]
pub struct InitStateCommand<C: ChainSpecParser> {
    #[command(flatten)]
    pub env: EnvironmentArgs<C>,

    /// JSONL file with state dump.
    ///
    /// Must contain accounts in following format, additional account fields are ignored. Must
    /// also contain { "root": \<state-root\> } as first line.
    /// {
    ///     "balance": "\<balance\>",
    ///     "nonce": \<nonce\>,
    ///     "code": "\<bytecode\>",
    ///     "storage": {
    ///         "\<key\>": "\<value\>",
    ///         ..
    ///     },
    ///     "address": "\<address\>",
    /// }
    ///
    /// Allows init at a non-genesis block. Caution! Blocks must be manually imported up until
    /// and including the non-genesis block to init chain at. See 'import' command.
    #[arg(value_name = "STATE_DUMP_FILE", verbatim_doc_comment)]
    pub state: PathBuf,

    /// Specifies whether to initialize the state without relying on EVM historical data.
    ///
    /// When enabled, and before inserting the state, it creates a dummy chain up to the last EVM
    /// block specified. It then appends the first provided block.
    ///
    /// - **Note**: **Do not** import receipts and blocks beforehand, or this will fail or be
    ///   ignored.
    #[arg(long, default_value = "false")]
    pub without_evm: bool,

    /// Header file containing the header in an RLP encoded format.
    #[arg(long, value_name = "HEADER_FILE", verbatim_doc_comment)]
    pub header: Option<PathBuf>,

    /// Hash of the header.
    #[arg(long, value_name = "HEADER_HASH", verbatim_doc_comment)]
    pub header_hash: Option<B256>,
}

impl<C: ChainSpecParser> InitStateCommand<C> {
    /// Execute the `init` command
    pub async fn execute(self, runtime: reth_tasks::Runtime) -> eyre::Result<()> {
        info!(target: "reth::cli", "Reth init-state starting");

        let Environment { config, provider_factory, .. } =
            self.env.init(AccessRights::RW, runtime)?;

        let static_file_provider = provider_factory.static_file_provider();

        if self.without_evm {
            let provider_rw = provider_factory.database_provider_rw()?;

            // ensure header, total difficulty and header hash are provided
            let header = self.header.ok_or_else(|| eyre::eyre!("Header file must be provided"))?;
            let header =
                without_evm::read_header_from_file::<base_common_types_chain::Header>(&header)?;

            let header_hash = self.header_hash.unwrap_or_else(|| header.hash_slow());

            let last_block_number = provider_rw.last_block_number()?;

            if last_block_number == 0 {
                without_evm::setup_without_evm(
                    &provider_rw,
                    SealedHeader::new(header, header_hash),
                    |number| {
                        let mut header = <base_common_types_chain::Header>::default();
                        header.set_number(number);
                        header
                    },
                )?;

                // SAFETY: it's safe to commit static files, since in the event of a crash, they
                // will be unwound according to database checkpoints.
                //
                // Necessary to commit, so the header is accessible to init_from_state_dump
                static_file_provider.commit()?;
            } else if last_block_number > 0 && last_block_number < header.number() {
                return Err(eyre::eyre!(
                    "Data directory should be empty when calling init-state with --without-evm."
                ));
            }

            provider_rw.commit()?;
        }

        info!(target: "reth::cli", "Initiating state dump");

        let reader = BufReader::new(reth_fs_util::open(self.state)?);

        let hash = init_from_state_dump(reader, &provider_factory, config.stages.etl)?;

        info!(target: "reth::cli", hash = ?hash, "Genesis block written");
        Ok(())
    }
}

impl<C: ChainSpecParser> InitStateCommand<C> {
    /// Returns the underlying chain being used to run this command
    pub fn chain_spec(&self) -> Option<&Arc<BaseChainSpec>> {
        Some(&self.env.chain)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::b256;

    use super::*;
    use crate::test_utils::BaseTestChainSpecParser;

    #[test]
    fn parse_init_state_command_with_without_evm() {
        let cmd: InitStateCommand<BaseTestChainSpecParser> = InitStateCommand::parse_from([
            "reth",
            "--chain",
            "base-sepolia",
            "--without-evm",
            "--header",
            "header.rlp",
            "--header-hash",
            "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
            "state.jsonl",
        ]);
        assert_eq!(cmd.state.to_str().unwrap(), "state.jsonl");
        assert!(cmd.without_evm);
        assert_eq!(cmd.header.unwrap().to_str().unwrap(), "header.rlp");
        assert_eq!(
            cmd.header_hash.unwrap(),
            b256!("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
        );
    }
}
