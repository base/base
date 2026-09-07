//! Utils for testing purposes.

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use alloy_consensus::Block;
use alloy_primitives::U256;
use alloy_rpc_types::engine::PayloadId;
use base_common_consensus::BaseTxEnvelope;
use reth_chain_state::CanonStateNotification;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::{BaseBuiltPayload, BasePayloadBuilderAttributes, PayloadKind};
use reth_primitives_traits::Block as _;

use crate::{
    PayloadBuilderHandle, PayloadBuilderService, PayloadJob, PayloadJobGenerator,
    service::BuildNewPayload, traits::KeepPayloadJobAlive,
};

/// Creates a new [`PayloadBuilderService`] for testing purposes.
pub fn test_payload_service() -> (
    PayloadBuilderService<
        TestPayloadJobGenerator,
        futures_util::stream::Empty<CanonStateNotification>,
    >,
    PayloadBuilderHandle,
) {
    PayloadBuilderService::new(Default::default(), futures_util::stream::empty())
}

/// Creates a new [`PayloadBuilderService`] for testing purposes and spawns it in the background.
pub fn spawn_test_payload_service() -> PayloadBuilderHandle {
    let (service, handle) = test_payload_service();
    tokio::spawn(service);
    handle
}

/// A [`PayloadJobGenerator`] for testing purposes
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct TestPayloadJobGenerator;

impl PayloadJobGenerator for TestPayloadJobGenerator {
    type Job = TestPayloadJob;

    fn new_payload_job(
        &self,
        input: BuildNewPayload<BasePayloadBuilderAttributes<BaseTxEnvelope>>,
        _id: PayloadId,
    ) -> Result<Self::Job, PayloadBuilderError> {
        Ok(TestPayloadJob { attr: input.attributes })
    }
}

/// A [`PayloadJob`] for testing purposes
#[derive(Debug)]
pub struct TestPayloadJob {
    attr: BasePayloadBuilderAttributes<BaseTxEnvelope>,
}

impl Future for TestPayloadJob {
    type Output = Result<(), PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

impl PayloadJob for TestPayloadJob {
    type PayloadAttributes = BasePayloadBuilderAttributes<BaseTxEnvelope>;
    type ResolvePayloadFuture =
        futures_util::future::Ready<Result<BaseBuiltPayload, PayloadBuilderError>>;
    type BuiltPayload = BaseBuiltPayload;

    fn best_payload(&self) -> Result<BaseBuiltPayload, PayloadBuilderError> {
        Ok(BaseBuiltPayload::new(
            self.attr.payload_attributes.id,
            Arc::new(Block::<_>::default().seal_slow()),
            U256::ZERO,
            None,
            None,
        ))
    }

    fn payload_attributes(
        &self,
    ) -> Result<BasePayloadBuilderAttributes<BaseTxEnvelope>, PayloadBuilderError> {
        Ok(self.attr.clone())
    }

    fn payload_timestamp(&self) -> Result<u64, PayloadBuilderError> {
        Ok(self.attr.payload_attributes.timestamp)
    }

    fn resolve_kind(
        &mut self,
        _kind: PayloadKind,
    ) -> (Self::ResolvePayloadFuture, KeepPayloadJobAlive) {
        let fut = futures_util::future::ready(self.best_payload());
        (fut, KeepPayloadJobAlive::No)
    }
}
