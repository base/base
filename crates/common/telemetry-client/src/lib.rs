#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{
    DEFAULT_QUEUE_CAPACITY, DEFAULT_REPORT_INTERVAL, DEFAULT_REQUEST_TIMEOUT,
    DEFAULT_SAMPLE_INTERVAL, GIT_SHA, MAX_LATENCY_SAMPLES, TELEMETRY_ID_FILE_NAME, TelemetryConfig,
};

mod identity;
pub use identity::{TelemetryId, TelemetryIdError};

mod hardware;
pub use hardware::{HardwareCollector, MountEntry};

mod builder;
pub use builder::{NodeIdentity, NodeReportBuilder};

mod sampler;
pub use sampler::{LatencySampler, LatencyWindow};

mod sink;
#[cfg(any(test, feature = "test-utils"))]
pub use sink::MockReportSink;
pub use sink::{HttpReportSink, ReportSink, ReportSinkError};

mod reporter;
pub use reporter::{DeliveryStreak, TelemetryReporter};

mod metrics;
pub use metrics::Metrics;
