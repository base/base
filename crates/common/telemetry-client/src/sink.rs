//! Where a built report goes.

use std::time::Duration;

use async_trait::async_trait;
use base_telemetry_types::NodeReport;
use url::Url;

/// Failures encountered while delivering a report.
#[derive(Debug, thiserror::Error)]
pub enum ReportSinkError {
    /// The request could not be built or sent.
    #[error("telemetry request failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// The endpoint answered, but not with success.
    #[error("telemetry endpoint returned HTTP {status}")]
    Status {
        /// The HTTP status code returned by the endpoint.
        status: u16,
    },
    /// The sink could not accept the report for a reason of its own.
    #[error("telemetry sink rejected the report: {0}")]
    Rejected(String),
}

impl ReportSinkError {
    /// Returns whether retrying this failure could plausibly succeed.
    ///
    /// A 4xx other than 429 means this build is sending something the endpoint will never
    /// accept, and retrying it is a waste of the node's time and ours.
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Status { status } => *status == 429 || *status >= 500,
            Self::Rejected(_) => false,
        }
    }

    /// Returns a stable label for this failure, for use as a metric or log field.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Transport(_) => "transport",
            Self::Status { .. } => "status",
            Self::Rejected(_) => "rejected",
        }
    }
}

/// The delivery seam for a built report.
///
/// Implementations must be cheap to clone-free share and must not panic: the reporter treats
/// every error as data, never as a reason to stop.
#[async_trait]
#[cfg_attr(any(test, feature = "test-utils"), mockall::automock)]
pub trait ReportSink: std::fmt::Debug + Send + Sync + 'static {
    /// Delivers one report.
    async fn send(&self, report: &NodeReport) -> Result<(), ReportSinkError>;
}

/// Delivers reports as JSON over HTTP to an ingest endpoint.
#[derive(Debug, Clone)]
pub struct HttpReportSink {
    client: reqwest::Client,
    endpoint: Url,
}

impl HttpReportSink {
    /// Builds a sink that POSTs to `endpoint`.
    ///
    /// The client deliberately does not call `.no_proxy()`, so `HTTPS_PROXY` and `NO_PROXY` are
    /// honored for operators who route egress through a proxy.
    pub fn new(endpoint: Url, request_timeout: Duration) -> Result<Self, ReportSinkError> {
        let client = reqwest::Client::builder().timeout(request_timeout).build()?;
        Ok(Self { client, endpoint })
    }

    /// Returns the endpoint this sink posts to.
    pub const fn endpoint(&self) -> &Url {
        &self.endpoint
    }
}

#[async_trait]
impl ReportSink for HttpReportSink {
    async fn send(&self, report: &NodeReport) -> Result<(), ReportSinkError> {
        let response = self.client.post(self.endpoint.clone()).json(report).send().await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(ReportSinkError::Status { status: status.as_u16() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_errors_and_throttling_are_retryable() {
        assert!(ReportSinkError::Status { status: 500 }.is_retryable());
        assert!(ReportSinkError::Status { status: 503 }.is_retryable());
        assert!(
            ReportSinkError::Status { status: 429 }.is_retryable(),
            "a throttled node should back off and try again, not give up"
        );
    }

    #[test]
    fn test_client_errors_are_not_retryable() {
        assert!(
            !ReportSinkError::Status { status: 400 }.is_retryable(),
            "retrying a payload the endpoint will never accept wastes the node's time"
        );
        assert!(!ReportSinkError::Status { status: 413 }.is_retryable());
        assert!(!ReportSinkError::Rejected("no".to_string()).is_retryable());
    }

    #[test]
    fn test_error_kinds_are_stable_labels() {
        assert_eq!(ReportSinkError::Status { status: 500 }.kind(), "status");
        assert_eq!(ReportSinkError::Rejected("no".to_string()).kind(), "rejected");
    }
}
