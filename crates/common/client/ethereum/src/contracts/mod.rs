//! Interact with on-chain contracts.

mod eth_call;
pub use eth_call::{CallDecoder, EthCall};

mod storage_slot;
pub use storage_slot::*;

mod error;
pub use error::{Error, Result, TransportErrorExt, TryParseTransportErrorResult};

mod event;
#[cfg(feature = "pubsub")]
pub use event::subscription::EventSubscription;
pub use event::{ChunkedEvent, Event, EventPoller};

mod interface;
pub use interface::*;

mod instance;
pub use instance::*;

mod call;
pub use call::*;

mod multicall;

// Not public API.
// NOTE: please avoid changing the API of this module due to its use in the `sol!` macro.
#[doc(hidden)]
pub mod private {
    pub use crate::{Ethereum, Network, Provider};
}
