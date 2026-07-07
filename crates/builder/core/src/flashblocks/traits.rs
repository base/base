//! Contains the payload builder trait.

use reth_payload_builder::PayloadBuilderError;
use reth_payload_primitives::{BuiltPayload, PayloadAttributes};
use tokio::sync::watch;

use crate::BuildArguments;

/// A trait for building payloads that encapsulate Ethereum transactions.
///
/// This trait provides the `try_build` method to construct a transaction payload
/// using `BuildArguments`. It returns a `Result` indicating success or a
/// `PayloadBuilderError` if building fails.
#[async_trait::async_trait]
pub trait PayloadBuilder: Send + Sync + Clone {
    /// The payload attributes type to accept for building.
    type Attributes: PayloadAttributes;
    /// The type of the built payload.
    type BuiltPayload: BuiltPayload;

    /// Tries to build a transaction payload using provided arguments.
    ///
    /// Constructs a transaction payload based on the given arguments,
    /// returning a `Result` indicating success or an error if building fails.
    ///
    /// # Arguments
    ///
    /// - `args`: Build arguments containing necessary components.
    /// - `payload_tx`: Watch sender; send the finalized payload here when ready.
    ///   Dropping it without sending signals failure to [`ResolvePayload`].
    ///
    /// # Returns
    ///
    /// A `Result` indicating the build outcome or an error.
    async fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
        payload_tx: watch::Sender<Option<Self::BuiltPayload>>,
    ) -> Result<(), PayloadBuilderError>;
}
