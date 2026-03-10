#![doc = include_str!("../README.md")]

mod auth;
pub use auth::*;

mod client;
pub use client::*;

mod filter;
pub use filter::*;

mod metrics;
pub use metrics::*;

mod rate_limit;
pub use rate_limit::*;

mod registry;
pub use registry::*;

mod server;
pub use server::*;

mod subscriber;
pub use subscriber::*;

/// Position of a flashblock entry in the stream as `(block_number, flashblock_index)`.
///
/// An equivalent alias exists in `base-builder-publish` for the upstream publisher crate.
/// Both are intentionally separate to avoid coupling a domain concept into the
/// generic `base-ring-buffer` crate or introducing a shared crate for a single
/// type alias that provides no additional type safety over the underlying tuple.
pub type FlashblockPosition = (u64, u64);

/// Convenience alias for the ring buffer used by the flashblocks proxy.
pub type FlashblocksRingBuffer = base_ring_buffer::RingBuffer<FlashblockPosition, bytes::Bytes>;

/// A broadcast entry carrying an optional position alongside its payload.
pub type PositionedMessage = (Option<FlashblockPosition>, axum::extract::ws::Message);
