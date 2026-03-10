//! Core transaction manager implementation.

use alloy_primitives::Address;

use crate::{SendHandle, SendResponse, TxCandidate, TxManager};

/// Default transaction manager implementation.
#[derive(Debug)]
pub struct SimpleTxManager;

impl TxManager for SimpleTxManager {
    async fn send(&self, _candidate: TxCandidate) -> SendResponse {
        todo!("SimpleTxManager::send")
    }

    async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
        todo!("SimpleTxManager::send_async")
    }

    fn sender_address(&self) -> Address {
        todo!("SimpleTxManager::sender_address")
    }
}
