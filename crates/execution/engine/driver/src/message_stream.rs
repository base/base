use std::path::PathBuf;

use base_common_chain_config::ChainSpecProvider;
use base_common_types_payload::ExecutionCommand;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_payload::BaseEngineValidator;
use futures::Stream;
use tokio_util::either::Either;

use crate::{EngineReorg, EngineSkipHeads, EngineSkipImport, EngineStoreStream};

/// Configures debugging and recording of execution-driver messages.
#[derive(Debug)]
pub struct EngineMessageStream;

impl EngineMessageStream {
    /// Skips fork-choice messages when a skip count is configured.
    pub const fn skip_heads<S: Stream<Item = ExecutionCommand>>(
        stream: S,
        count: Option<usize>,
    ) -> Either<EngineSkipHeads<S>, S> {
        match count {
            Some(count) => Either::Left(EngineSkipHeads::new(stream, count)),
            None => Either::Right(stream),
        }
    }

    /// Skips payload messages when a skip count is configured.
    pub const fn skip_import<S: Stream<Item = ExecutionCommand>>(
        stream: S,
        count: Option<usize>,
    ) -> Either<EngineSkipImport<S>, S> {
        match count {
            Some(count) => Either::Left(EngineSkipImport::new(stream, count)),
            None => Either::Right(stream),
        }
    }

    /// Records messages after preceding filters have run, when a directory is configured.
    pub fn store<S: Stream<Item = ExecutionCommand>>(
        stream: S,
        path: Option<PathBuf>,
    ) -> Either<EngineStoreStream<S>, S> {
        match path {
            Some(path) => Either::Left(EngineStoreStream::new(stream, path)),
            None => Either::Right(stream),
        }
    }

    /// Injects synthetic reorgs when a frequency is configured.
    pub fn reorg<S: Stream<Item = ExecutionCommand>, P: ChainSpecProvider>(
        stream: S,
        provider: P,
        evm_config: BaseEvmConfig,
        frequency: Option<usize>,
        depth: Option<usize>,
    ) -> Either<EngineReorg<S, P>, S> {
        match frequency {
            Some(frequency) => {
                let validator = BaseEngineValidator::new(provider.chain_spec());
                Either::Left(EngineReorg::new(
                    stream,
                    provider,
                    evm_config,
                    validator,
                    frequency,
                    depth.unwrap_or_default(),
                ))
            }
            None => Either::Right(stream),
        }
    }
}
