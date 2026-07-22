#![doc = include_str!("../README.md")]

mod error;
pub use error::PricingError;

mod rate;
pub use rate::Rate;

mod feed;
pub use feed::{AnswerShape, FeedConfig, FeedDirection, FeedReading};

mod slot;
pub use slot::{SlotField, SlotFeed, SlotTimestamp};

mod config;
pub use config::{PayerConfig, PriceSource, TokenConfig};

mod snapshot;
pub use snapshot::{Erc20, PriceSnapshot, TokenPrice};

#[cfg(feature = "storage")]
mod storage;
#[cfg(feature = "storage")]
pub use storage::PayerConfigStorage;

#[cfg(feature = "signer")]
mod signer;
#[cfg(feature = "signer")]
pub use signer::{LocalPayerSigner, PayerCosigner, PayerDigestSigner, PayerSignerError};
