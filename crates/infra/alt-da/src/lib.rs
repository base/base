#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use base_protocol::BLOB_MAX_DATA_SIZE;

/// Max batch blob bytes accepted on PUT and loaded from the backing store.
/// Sized for eight max-size blob frames per channel submission.
pub const MAX_OBJECT_BYTES: usize = 8 * BLOB_MAX_DATA_SIZE;

mod commitment;
pub use commitment::{decode_hex_commitment, generate_generic_commitment, object_key, object_name};

mod error;
pub use error::{ConfigError, Error, InternalError, StoreError};

mod server;
pub use server::{Config, Server};

mod store;
pub use store::{DynStore, FileStore, S3Store, Store, StoreOpener};
