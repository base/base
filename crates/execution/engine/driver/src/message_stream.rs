use std::path::PathBuf;

use base_common_chain_config::ChainSpecProvider;
use base_execution_engine_types::BeaconEngineMessage;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_payload_builder::BaseEngineValidator;
use futures::Stream;
use tokio_util::either::Either;

use crate::EngineStoreStream;

use crate::EngineSkipFcu;

use crate::EngineSkipNewPayload;

use crate::EngineReorg;

/// The collection of stream extensions for engine API message stream.
pub trait EngineMessageStreamExt: Stream<Item = BeaconEngineMessage> {
    /// Skips the specified number of [`BeaconEngineMessage::ForkchoiceUpdated`] messages from the
    /// engine message stream.
    fn skip_fcu(self, count: usize) -> EngineSkipFcu<Self>
    where
        Self: Sized,
    {
        EngineSkipFcu::new(self, count)
    }

    /// If the count is [Some], returns the stream that skips the specified number of
    /// [`BeaconEngineMessage::ForkchoiceUpdated`] messages. Otherwise, returns `Self`.
    fn maybe_skip_fcu(self, maybe_count: Option<usize>) -> Either<EngineSkipFcu<Self>, Self>
    where
        Self: Sized,
    {
        if let Some(count) = maybe_count {
            Either::Left(self.skip_fcu(count))
        } else {
            Either::Right(self)
        }
    }

    /// Skips the specified number of [`BeaconEngineMessage::NewPayload`] messages from the
    /// engine message stream.
    fn skip_new_payload(self, count: usize) -> EngineSkipNewPayload<Self>
    where
        Self: Sized,
    {
        EngineSkipNewPayload::new(self, count)
    }

    /// If the count is [Some], returns the stream that skips the specified number of
    /// [`BeaconEngineMessage::NewPayload`] messages. Otherwise, returns `Self`.
    fn maybe_skip_new_payload(
        self,
        maybe_count: Option<usize>,
    ) -> Either<EngineSkipNewPayload<Self>, Self>
    where
        Self: Sized,
    {
        if let Some(count) = maybe_count {
            Either::Left(self.skip_new_payload(count))
        } else {
            Either::Right(self)
        }
    }

    /// Stores engine messages at the specified location.
    fn store_messages(self, path: PathBuf) -> EngineStoreStream<Self>
    where
        Self: Sized,
    {
        EngineStoreStream::new(self, path)
    }

    /// If the path is [Some], returns the stream that stores engine messages at the specified
    /// location. Otherwise, returns `Self`.
    fn maybe_store_messages(
        self,
        maybe_path: Option<PathBuf>,
    ) -> Either<EngineStoreStream<Self>, Self>
    where
        Self: Sized,
    {
        if let Some(path) = maybe_path {
            Either::Left(self.store_messages(path))
        } else {
            Either::Right(self)
        }
    }

    /// Creates reorgs with specified frequency.
    fn reorg<Provider>(
        self,
        provider: Provider,
        evm_config: BaseEvmConfig,
        payload_validator: BaseEngineValidator,
        frequency: usize,
        depth: Option<usize>,
    ) -> EngineReorg<Self, Provider>
    where
        Self: Sized,
    {
        EngineReorg::new(
            self,
            provider,
            evm_config,
            payload_validator,
            frequency,
            depth.unwrap_or_default(),
        )
    }

    /// Adds synthetic reorgs when a frequency is configured.
    fn maybe_reorg<Provider: ChainSpecProvider>(
        self,
        provider: Provider,
        evm_config: BaseEvmConfig,
        frequency: Option<usize>,
        depth: Option<usize>,
    ) -> Either<EngineReorg<Self, Provider>, Self>
    where
        Self: Sized,
    {
        if let Some(frequency) = frequency {
            let validator = BaseEngineValidator::new(provider.chain_spec());
            Either::Left(EngineReorg::new(
                self,
                provider,
                evm_config,
                validator,
                frequency,
                depth.unwrap_or_default(),
            ))
        } else {
            Either::Right(self)
        }
    }
}

impl<S> EngineMessageStreamExt for S where S: Stream<Item = BeaconEngineMessage> {}
