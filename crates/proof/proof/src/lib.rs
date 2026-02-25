#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]

extern crate alloc;

#[macro_use]
extern crate tracing;

mod l1;
pub use l1::{
    OracleBlobProvider, OracleL1ChainProvider, OraclePipeline, ProviderAttributesBuilder,
    ProviderDerivationPipeline, ROOTS_OF_UNITY,
};

mod l2;
pub use l2::OracleL2ChainProvider;

mod sync;
pub use sync::new_oracle_pipeline_cursor;

mod errors;
pub use errors::{HintParsingError, OracleProviderError};

mod executor;
pub use executor::KonaExecutor;

mod hint;
pub use hint::{Hint, HintType};

mod boot;
pub use boot::{
    BootInfo, L1_CONFIG_KEY, L1_HEAD_KEY, L2_CHAIN_ID_KEY, L2_CLAIM_BLOCK_NUMBER_KEY, L2_CLAIM_KEY,
    L2_OUTPUT_ROOT_KEY, L2_ROLLUP_CONFIG_KEY,
};

mod caching_oracle;
pub use caching_oracle::{CachingOracle, FlushableCache};

mod blocking_runtime;
pub use blocking_runtime::block_on;

mod eip2935;
pub use eip2935::eip_2935_history_lookup;
