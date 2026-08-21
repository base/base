//! Reusable deterministic fixtures for proof-backend tests.

mod denim;
pub use denim::{
    CLAIM_BLOCK, DENIM_CHAIN_ID, DENIM_CONFIG_HASH, DENIM_FIXTURE_CONTENT_HASH, DENIM_TIMESTAMP,
    DenimFixture, ExpectedDenimBlock,
};
