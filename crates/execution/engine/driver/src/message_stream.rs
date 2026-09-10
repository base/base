use std::path::PathBuf;

use base_common_chain_config::ChainSpecProvider;
use base_execution_engine_types::BeaconEngineMessage;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_payload_builder::BaseEngineValidator;
use futures::Stream;
use tokio_util::either::Either;

use crate::{EngineReorg, EngineSkipFcu, EngineSkipNewPayload, EngineStoreStream};

/// Configures debugging and recording of execution-driver messages.
#[derive(Debug)]
pub struct EngineMessageStream;

impl EngineMessageStream {
    /// Skips fork-choice messages when a skip count is configured.
    pub fn skip_fcu<S: Stream<Item = BeaconEngineMessage>>(
        stream: S,
        count: Option<usize>,
    ) -> Either<EngineSkipFcu<S>, S> {
        match count {
            Some(count) => Either::Left(EngineSkipFcu::new(stream, count)),
            None => Either::Right(stream),
        }
    }

    /// Skips payload messages when a skip count is configured.
    pub fn skip_new_payload<S: Stream<Item = BeaconEngineMessage>>(
        stream: S,
        count: Option<usize>,
    ) -> Either<EngineSkipNewPayload<S>, S> {
        match count {
            Some(count) => Either::Left(EngineSkipNewPayload::new(stream, count)),
            None => Either::Right(stream),
        }
    }

    /// Records messages after preceding filters have run, when a directory is configured.
    pub fn store<S: Stream<Item = BeaconEngineMessage>>(
        stream: S,
        path: Option<PathBuf>,
    ) -> Either<EngineStoreStream<S>, S> {
        match path {
            Some(path) => Either::Left(EngineStoreStream::new(stream, path)),
            None => Either::Right(stream),
        }
    }

    /// Injects synthetic reorgs when a frequency is configured.
    pub fn reorg<S: Stream<Item = BeaconEngineMessage>, P: ChainSpecProvider>(
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
