//! Core transaction manager implementation.

use alloy_primitives::Address;
use alloy_rpc_types_eth::TransactionReceipt;
use tokio::sync::oneshot;

use crate::{SendResponse, TxCandidate, TxManager, TxManagerError};

/// Default transaction manager implementation.
#[derive(Debug)]
pub struct SimpleTxManager;

impl TxManager for SimpleTxManager {
    async fn send(&self, _candidate: TxCandidate) -> Result<TransactionReceipt, TxManagerError> {
        todo!("SimpleTxManager::send")
    }

    async fn send_async(&self, _candidate: TxCandidate) -> oneshot::Receiver<SendResponse> {
        todo!("SimpleTxManager::send_async")
    }

    fn sender_address(&self) -> Address {
        todo!("SimpleTxManager::sender_address")
    }
}
