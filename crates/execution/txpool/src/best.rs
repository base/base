use std::sync::Arc;

use reth_transaction_pool::{
    BestTransactions, ValidPoolTransaction, error::InvalidPoolTransactionError,
};

use crate::BasePooledTx;

/// Merges best-transaction iterators from the protocol pool and the 2D nonce sidecar.
pub(crate) struct MergeBestTransactions<T: BasePooledTx> {
    protocol: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
    sidecar: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
    next_protocol: Option<Arc<ValidPoolTransaction<T>>>,
    next_sidecar: Option<Arc<ValidPoolTransaction<T>>>,
}

impl<T: BasePooledTx> MergeBestTransactions<T> {
    /// Creates a merged iterator from the protocol pool and 2D nonce sidecar.
    pub(crate) fn new(
        protocol: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
        sidecar: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
    ) -> Self {
        Self { protocol, sidecar, next_protocol: None, next_sidecar: None }
    }

    fn protocol_is_better(
        protocol: &Arc<ValidPoolTransaction<T>>,
        sidecar: &Arc<ValidPoolTransaction<T>>,
    ) -> bool {
        protocol.transaction.max_fee_per_gas() >= sidecar.transaction.max_fee_per_gas()
    }
}

impl<T: BasePooledTx> std::fmt::Debug for MergeBestTransactions<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeBestTransactions").finish_non_exhaustive()
    }
}

impl<T: BasePooledTx> Iterator for MergeBestTransactions<T> {
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_protocol.is_none() {
            self.next_protocol = self.protocol.next();
        }
        if self.next_sidecar.is_none() {
            self.next_sidecar = self.sidecar.next();
        }

        match (&self.next_protocol, &self.next_sidecar) {
            (Some(protocol), Some(sidecar)) => {
                if Self::protocol_is_better(protocol, sidecar) {
                    self.next_protocol.take()
                } else {
                    self.next_sidecar.take()
                }
            }
            (Some(_), None) => self.next_protocol.take(),
            (None, Some(_)) => self.next_sidecar.take(),
            (None, None) => None,
        }
    }
}

impl<T: BasePooledTx> BestTransactions for MergeBestTransactions<T> {
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: &InvalidPoolTransactionError) {
        if transaction.transaction.eip8130_nonce_channel_key().is_some() {
            self.sidecar.mark_invalid(transaction, kind);
        } else {
            self.protocol.mark_invalid(transaction, kind);
        }
    }

    fn no_updates(&mut self) {
        self.protocol.no_updates();
        self.sidecar.no_updates();
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.protocol.set_skip_blobs(skip_blobs);
        self.sidecar.set_skip_blobs(skip_blobs);
    }
}
