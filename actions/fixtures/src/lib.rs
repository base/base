#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use dotenvy as _;
use tokio as _;

mod fixture;
pub use fixture::{
    ActionFixture, BlockId, CURRENT_SCHEMA_VERSION, DerivationFixture, ExpectedOutcome,
    ExpectedPayload, FixtureBlob, FixtureKind, FixtureKindParseError, FixtureL1Block,
    FixtureL1DiskBlock, FixtureL1DiskBlockError, FixtureL1DiskCodec, FixtureL2Block,
    FixtureManifest, FixturePaths, StateRoot,
};

mod validation;
pub use validation::{FixtureBlockId, FixtureValidationError, FixtureValidator};

mod loader;
pub use loader::{FixtureLoader, FixtureLoaderError};

mod catalog;
pub use catalog::{ActionFixtureCatalog, FixtureCatalogEntry, FixtureCatalogError};

mod adapter;
pub use adapter::{ActionFixtureAdapter, FixtureAdapterError};

mod replay;
pub use replay::{DerivationFixtureReplayer, FixtureReplayError};

mod capture;
pub use capture::{
    CaptureCommand, CaptureError, CaptureInput, CaptureOutput, FIXTURE_CAPTURE_RPC_TIMEOUT,
    L1_DERIVATION_CAPTURE_CHUNK_SIZE, L1_DERIVATION_CAPTURE_CONCURRENCY, RpcFixtureCapture,
};
