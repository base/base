#![doc = include_str!("../README.md")]

mod error;
pub use error::{Result, TdxRuntimeError};

mod signer;
pub use signer::{SignerIdentity, TdxSigner};

mod report_data;
pub use report_data::{TDX_REPORT_DATA_LEN, TdxReportData};

mod quote;
pub use quote::{ConfigfsTdxQuoteProvider, TdxQuoteProvider};

mod runtime;
pub use runtime::{TdxRuntime, TdxSignerQuote};
