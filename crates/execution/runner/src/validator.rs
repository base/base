use std::{fmt, sync::Arc};

use base_engine_tree::OnValidatedBlockHook;
use base_node_core::OpEngineValidatorBuilder;
use reth_consensus::ConsensusError;
use reth_node_api::{
    AddOnsContext, EngineApiValidator, ExecutionPayload, FullNodeComponents, PayloadAttributes,
    PayloadValidator,
    payload::{
        EngineApiMessageVersion, EngineObjectValidationError, NewPayloadError, PayloadOrAttributes,
        PayloadTypes,
    },
};
use reth_node_builder::rpc::PayloadValidatorBuilder;
use reth_primitives_traits::{RecoveredBlock, SealedBlock};
use reth_trie_common::HashedPostState;
use tokio::runtime::Handle;
use tracing::info;

/// A payload validator that wraps [`OpEngineValidator`](base_node_core::engine::OpEngineValidator)
/// and stores an optional [`OnValidatedBlockHook`] to wait for other processes to finish on
/// validated blocks.
///
/// This mirrors the hook pattern used by
/// [`BaseEngineValidator`](base_engine_tree::BaseEngineValidator) but at the payload validation
/// level.
pub struct BasePayloadValidator<V> {
    /// The inner payload validator.
    inner: V,
    /// Hook invoked after a block has been validated.
    on_validated_block: Option<Arc<dyn OnValidatedBlockHook>>,
}

impl<V: fmt::Debug> fmt::Debug for BasePayloadValidator<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BasePayloadValidator")
            .field("inner", &self.inner)
            .field("on_validated_block", &self.on_validated_block.as_ref().map(|_| "..."))
            .finish()
    }
}

impl<V> BasePayloadValidator<V> {
    /// Creates a new [`BasePayloadValidator`] wrapping the given validator.
    pub fn new(inner: V, on_validated_block: Option<Arc<dyn OnValidatedBlockHook>>) -> Self {
        Self { inner, on_validated_block }
    }

    /// Returns a reference to the inner payload validator.
    pub fn inner(&self) -> &V {
        &self.inner
    }

    /// Returns the [`OnValidatedBlockHook`], if configured.
    pub fn on_validated_block_hook(&self) -> Option<&Arc<dyn OnValidatedBlockHook>> {
        self.on_validated_block.as_ref()
    }
}

impl<V: Clone> Clone for BasePayloadValidator<V> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone(), on_validated_block: self.on_validated_block.clone() }
    }
}

impl<V, Types> PayloadValidator<Types> for BasePayloadValidator<V>
where
    V: PayloadValidator<Types>,
    Types: PayloadTypes,
{
    type Block = V::Block;

    fn validate_block_post_execution_with_hashed_state(
        &self,
        state_updates: &HashedPostState,
        block: &RecoveredBlock<Self::Block>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_block_post_execution_with_hashed_state(state_updates, block)
    }

    fn convert_payload_to_block(
        &self,
        payload: Types::ExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        self.inner.convert_payload_to_block(payload)
    }
}

impl<V, Types> EngineApiValidator<Types> for BasePayloadValidator<V>
where
    V: EngineApiValidator<Types>,
    Types: PayloadTypes,
{
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<
            '_,
            Types::ExecutionData,
            <Types as PayloadTypes>::PayloadAttributes,
        >,
    ) -> Result<(), EngineObjectValidationError> {
        self.inner.validate_version_specific_fields(version, payload_or_attrs)
    }

    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &<Types as PayloadTypes>::PayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        info!(target: "base-payload-validator", has_hook = self.on_validated_block.is_some(), "Ensuring well-formed attributes");
        if let Some(hook) = &self.on_validated_block {
            info!(target: "base-payload-validator", "Invoking on_validated_block hook");
            hook.on_validated_block(attributes.timestamp());
            info!(target: "base-payload-validator", "On_validated_block hook invoked");
        }
        self.inner.ensure_well_formed_attributes(version, attributes)
    }
}

/// A builder for [`BasePayloadValidator`] that wraps an [`OpEngineValidatorBuilder`] and accepts
/// an optional [`OnValidatedBlockHook`].
#[derive(Clone)]
pub struct BasePayloadValidatorBuilder {
    /// The inner payload validator builder.
    inner: OpEngineValidatorBuilder,
    /// Hook invoked after a block has been validated.
    on_validated_block: Option<Arc<dyn OnValidatedBlockHook>>,
}

impl fmt::Debug for BasePayloadValidatorBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BasePayloadValidatorBuilder")
            .field("inner", &self.inner)
            .field("on_validated_block", &self.on_validated_block.as_ref().map(|_| "..."))
            .finish()
    }
}

impl BasePayloadValidatorBuilder {
    /// Creates a new [`BasePayloadValidatorBuilder`].
    pub fn new(inner: OpEngineValidatorBuilder) -> Self {
        Self { inner, on_validated_block: None }
    }

    /// Sets the [`OnValidatedBlockHook`] to invoke after block validation.
    pub fn with_on_validated_block(mut self, hook: Arc<dyn OnValidatedBlockHook>) -> Self {
        self.on_validated_block = Some(hook);
        self
    }
}

impl Default for BasePayloadValidatorBuilder {
    fn default() -> Self {
        Self::new(OpEngineValidatorBuilder::default())
    }
}

impl<Node> PayloadValidatorBuilder<Node> for BasePayloadValidatorBuilder
where
    OpEngineValidatorBuilder: PayloadValidatorBuilder<Node>,
    Node: FullNodeComponents,
{
    type Validator = BasePayloadValidator<
        <OpEngineValidatorBuilder as PayloadValidatorBuilder<Node>>::Validator,
    >;

    async fn build(self, ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        info!(target: "base-payload-validator", has_hook = self.on_validated_block.is_some(), "Building payload validator");
        let inner = self.inner.build(ctx).await?;
        if !self.on_validated_block.is_some() {
            panic!("on_validated_block hook is not set");
        }
        Ok(BasePayloadValidator::new(inner, self.on_validated_block))
    }
}
