#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]

extern crate alloc;

#[macro_use]
extern crate tracing;

mod l1;
pub use l1::*;

mod l2;
pub use l2::*;

mod sync;
pub use sync::{SafeHeadFetcher, new_oracle_pipeline_cursor};

mod errors;
pub use errors::*;

mod executor;
pub use executor::*;

mod hint;
pub use hint::{Hint, HintType};

mod boot;
pub use boot::*;

mod caching_oracle;
pub use caching_oracle::{CachingOracle, FlushableCache};

mod blocking_runtime;
pub use blocking_runtime::block_on;

mod eip2935;
pub use eip2935::eip_2935_history_lookup;
