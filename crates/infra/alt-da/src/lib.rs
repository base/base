#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

/// Max batch blob bytes accepted on PUT and loaded from the backing store.
///
/// Single source of truth: [`base_protocol::MAX_DA_OBJECT_BYTES`].
pub const MAX_OBJECT_BYTES: usize = base_protocol::MAX_DA_OBJECT_BYTES;

mod commitment;
pub use commitment::{
    GENERIC_COMMITMENT_LEN, GENERIC_COMMITMENT_SENTINEL, GENERIC_COMMITMENT_TYPE,
    GenericCommitment, decode_hex_commitment, encode_commitment_tx_data,
    generate_generic_commitment, object_key, object_name, validate_generic_commitment,
};

mod client;
pub use client::Client;

mod error;
pub use error::{ClientError, CommitmentError, ConfigError, Error, InternalError, StoreError};

mod server;
pub use server::{Config, Server};

mod store;
pub use store::{DynStore, FileStore, S3Store, Store, StoreOpener};
