//! Consensus safedb implementation.

mod error;
pub use error::SafeDBError;

pub use base_common_types_rpc::SafeHeadResponse;

mod traits;
pub use traits::{SafeDBReader, SafeHeadListener};

mod disabled;
pub use disabled::DisabledSafeDB;

mod db;
pub use db::SafeDB;
