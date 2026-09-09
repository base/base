#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

// Re-export tracing crates
pub use tracing;
#[cfg(feature = "std")]
pub use tracing_appender;
#[cfg(feature = "std")]
pub use tracing_subscriber;

#[cfg(all(feature = "tracy", feature = "std"))]
tracy_client::register_demangler!();

// Re-export our types
#[cfg(feature = "std")]
pub use formatter::LogFormat;
#[cfg(feature = "std")]
pub use layers::{FileInfo, FileWorkerGuard, Layers, TracingGuards};
#[cfg(feature = "std")]
pub use log_handle::{
    LogFilterReloadHandle, install_log_handle, log_handle_available, set_log_verbosity,
    set_log_vmodule,
};
#[cfg(feature = "std")]
pub use test_tracer::TestTracer;

#[cfg(feature = "std")]
#[doc(hidden)]
pub mod __private {
    pub use super::throttle::*;
}

#[cfg(feature = "std")]
mod formatter;
#[cfg(feature = "std")]
mod layers;
#[cfg(feature = "std")]
pub mod log_handle;
#[cfg(feature = "std")]
mod test_tracer;
#[cfg(feature = "std")]
mod throttle;

#[cfg(feature = "std")]
mod tracer;
#[cfg(feature = "std")]
pub use tracer::{LayerInfo, RethTracer, Tracer, init_test_tracing};

#[cfg(feature = "otlp")]
mod otlp;
#[cfg(feature = "otlp-logs")]
pub use otlp::log_layer;
#[cfg(feature = "otlp")]
pub use otlp::{OtlpConfig, OtlpLogsConfig, OtlpProtocol, span_layer};
