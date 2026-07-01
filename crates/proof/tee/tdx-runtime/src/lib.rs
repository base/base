#![doc = include_str!("../README.md")]

mod error;
pub use error::{Result, TdxRuntimeError};

mod signer;
pub use signer::{SignerIdentity, TdxSigner};

mod report_data;
pub use report_data::{TDX_REPORT_DATA_LEN, TDX_REPORT_DATA_SUFFIX_CONTEXT, TdxReportData};

mod quote;
pub use quote::{
    ConfigfsTdxQuoteProvider, DEFAULT_TSM_REPORT_ROOT, TDX_CONFIGFS_PROVIDER_NAME, TdxQuoteProvider,
};

mod runtime;
pub use runtime::{TdxRuntime, TdxSignerQuote};
