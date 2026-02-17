//! Backend implementations for Roxy.

mod config;
pub use config::BackendConfig;

mod http;
pub use http::HttpBackend;

mod traits;
pub use traits::Backend;
