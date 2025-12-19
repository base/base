use crate::{
    Mempool,
    domain::{ReputationService, ReputationStatus},
};
use alloy_primitives::Address;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct ReputationServiceImpl<T: Mempool> {
    mempool: Arc<RwLock<T>>,
}

impl<T: Mempool> ReputationServiceImpl<T> {
    pub fn new(mempool: Arc<RwLock<T>>) -> Self {
        Self { mempool }
    }
}

#[async_trait]
impl<T: Mempool> ReputationService for ReputationServiceImpl<T> {
    async fn get_reputation(&self, _entity: &Address) -> ReputationStatus {
        // DO something with the mempool for compiling reasons, as this is scafolding
        let _ = self.mempool.read().await.get_top_operations(1);
        ReputationStatus::Ok
    }
}
