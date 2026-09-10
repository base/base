//! Base metering RPC adapters.

mod rpc;
pub use rpc::MeteringApiImpl;
mod traits;
pub use traits::MeteringApiServer;
mod store_rpc;
pub use store_rpc::{BaseApiExtServer, MeteringStoreExt};
