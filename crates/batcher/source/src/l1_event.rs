//! L1 head event type.

/// An event describing a change in the observed L1 chain head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L1HeadEvent {
    /// A new L1 head block number was observed.
    NewHead(u64),
}
