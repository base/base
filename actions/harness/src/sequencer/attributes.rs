use std::sync::{Arc, Mutex};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::Bytes;
use async_trait::async_trait;
use base_common_consensus::BaseTxEnvelope;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_derive::{
    AttributesBuilder, PipelineError, PipelineResult, StatefulAttributesBuilder,
};
use base_protocol::L2BlockInfo;

use crate::{ActionL1ChainProvider, ActionL2ChainProvider};

/// Attributes builder adapter that injects one test-controlled transaction batch.
#[derive(Debug)]
pub struct ActionSequencerAttributesBuilder {
    inner: StatefulAttributesBuilder<ActionL1ChainProvider, ActionL2ChainProvider>,
    user_txs: Arc<Mutex<Option<Vec<BaseTxEnvelope>>>>,
}

impl ActionSequencerAttributesBuilder {
    /// Create a new attributes adapter.
    pub const fn new(
        inner: StatefulAttributesBuilder<ActionL1ChainProvider, ActionL2ChainProvider>,
        user_txs: Arc<Mutex<Option<Vec<BaseTxEnvelope>>>>,
    ) -> Self {
        Self { inner, user_txs }
    }
}

#[async_trait]
impl AttributesBuilder for ActionSequencerAttributesBuilder {
    async fn prepare_payload_attributes(
        &mut self,
        l2_parent: L2BlockInfo,
        epoch: alloy_eips::BlockNumHash,
    ) -> PipelineResult<BasePayloadAttributes> {
        let mut attrs = self.inner.prepare_payload_attributes(l2_parent, epoch).await?;
        let user_txs = self
            .user_txs
            .lock()
            .expect("sequencer user tx queue lock poisoned")
            .take()
            .ok_or_else(|| PipelineError::NotEnoughData.temp())?;
        let encoded_user_txs: Vec<Bytes> = user_txs
            .into_iter()
            .map(|tx| {
                let mut buf = Vec::new();
                tx.encode_2718(&mut buf);
                Bytes::from(buf)
            })
            .collect();
        if !encoded_user_txs.is_empty() {
            attrs.transactions.get_or_insert_with(Vec::new).extend(encoded_user_txs);
        }
        attrs.no_tx_pool = Some(true);
        Ok(attrs)
    }
}
