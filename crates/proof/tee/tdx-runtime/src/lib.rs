#![doc = include_str!("../README.md")]

mod error;
pub use error::{Result, TdxRuntimeError};

mod signer;
pub use signer::TdxSigner;

mod token;
pub use token::{
    CONFIDENTIAL_SPACE_AUDIENCE, ConfidentialSpaceTokenProvider, StaticTokenProvider,
    TdxAttestationTokenProvider,
};

mod runtime;
pub use runtime::{TdxAttestationContext, TdxRuntime};
