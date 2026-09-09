//! Batcher administration endpoints and server.

mod api;
pub use api::{BatcherAdminApiServer, BatcherAdminApiServerImpl};
mod server;
pub use server::AdminServer;
