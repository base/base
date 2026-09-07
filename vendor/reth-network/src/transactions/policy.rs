use std::fmt::Debug;

use crate::transactions::config::{AnnouncementFilteringPolicy, TransactionPropagationPolicy};

/// A container that bundles specific implementations of transaction-related policies,
///
/// This struct provides a complete set of policies required by components like the
/// [`TransactionsManager`](super::TransactionsManager). It holds a specific
/// [`TransactionPropagationPolicy`] and an [`AnnouncementFilteringPolicy`].
#[derive(Debug)]
pub struct NetworkPolicies {
    propagation: Box<dyn TransactionPropagationPolicy>,
    announcement: Box<dyn AnnouncementFilteringPolicy>,
}

impl NetworkPolicies {
    /// Creates a new bundle of network policies.
    pub fn new(
        propagation: impl TransactionPropagationPolicy,
        announcement: impl AnnouncementFilteringPolicy,
    ) -> Self {
        Self { propagation: Box::new(propagation), announcement: Box::new(announcement) }
    }

    /// Returns a new `NetworkPolicies` bundle with the `TransactionPropagationPolicy` replaced.
    pub fn with_propagation(self, new_propagation: impl TransactionPropagationPolicy) -> Self {
        Self { propagation: Box::new(new_propagation), announcement: self.announcement }
    }

    /// Returns a new `NetworkPolicies` bundle with the `AnnouncementFilteringPolicy` replaced.
    pub fn with_announcement(self, new_announcement: impl AnnouncementFilteringPolicy) -> Self {
        Self { propagation: self.propagation, announcement: Box::new(new_announcement) }
    }

    /// Returns a reference to the transaction propagation policy.
    pub fn propagation_policy(&self) -> &dyn TransactionPropagationPolicy {
        &*self.propagation
    }

    /// Returns a mutable reference to the transaction propagation policy.
    pub fn propagation_policy_mut(&mut self) -> &mut dyn TransactionPropagationPolicy {
        &mut *self.propagation
    }

    /// Returns a reference to the announcement filtering policy.
    pub fn announcement_filter(&self) -> &dyn AnnouncementFilteringPolicy {
        &*self.announcement
    }
}
