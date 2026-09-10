//! Consensus safedb implementation.

mod error;
pub use base_common_types_rpc::SafeHeadResponse;
pub use error::SafeDBError;

mod traits;
pub use traits::{SafeDBReader, SafeHeadListener};

mod disabled;
pub use disabled::DisabledSafeDB;

mod db;
pub use db::SafeDB;
