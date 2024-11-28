//! [`TransactionPoolBundleExt`] implementation generic over any bundle and network type.

use alloy_primitives::U256;
use alloy_rpc_types_beacon::events::PayloadAttributesEvent;
use reth_transaction_pool::TransactionPool;
use std::{fmt::Debug, future::Future};

/// Bundle-related operations.
///
/// This API is under active development.
pub trait BundlePoolOperations: Sync + Send {
    /// Bundle type
    type Bundle: Send;

    /// Cancel bundle request type
    type CancelBundleReq;

    /// Error type
    type Error: Debug;

    /// Transactions type
    type Transaction: Debug;

    /// Add a bundle to the pool, returning an Error if invalid.
    fn add_bundle(
        &self,
        bundle: Self::Bundle,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Make a best-effort attempt to cancel a bundle
    fn cancel_bundle(
        &self,
        hash: Self::CancelBundleReq,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Get transactions to be included in the head of the next block
    fn get_transactions(
        &self,
        slot: U256,
    ) -> Result<impl IntoIterator<Item = Self::Transaction>, Self::Error>;

    /// Notify new payload attributes to use
    fn notify_payload_attributes_event(
        &self,
        payload_attributes: PayloadAttributesEvent,
        gas_limit: Option<u64>,
    ) -> Result<(), Self::Error>;
}

/// Extension for [TransactionPool] trait adding support for [BundlePoolOperations].
pub trait TransactionPoolBundleExt: TransactionPool + BundlePoolOperations {}
