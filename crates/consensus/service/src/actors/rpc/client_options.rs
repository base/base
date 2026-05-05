//! Options shared by queued RPC clients.

use std::time::Duration;

/// Runtime options for queued in-process RPC clients.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QueuedRpcClientOptions {
    /// Optional deadline for waiting on actor response channels.
    ///
    /// `None` preserves the historical behavior: once a request is accepted, the caller waits
    /// until the actor responds or drops the response channel.
    pub request_timeout: Option<Duration>,
}
