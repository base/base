#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod profiler;
pub use profiler::{CpuProfiler, ProfilerError};

mod server;
pub use server::{ProfilingServer, ProfilingServerError};

mod extension;
pub use extension::{ProfilingConfig, ProfilingExtension};
