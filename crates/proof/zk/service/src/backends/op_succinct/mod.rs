//! OP-Succinct proving backend using the SP1 cluster.

mod backend;
pub use backend::OpSuccinctBackend;

mod provider;
pub use provider::OpSuccinctProvider;
