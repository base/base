//! Admin command channel for runtime control of the batch driver.

use tokio::sync::{mpsc, oneshot};

use crate::{ThrottleConfig, ThrottleInfo, ThrottleStrategy};

/// Capacity of the admin command channel.
///
/// 32 is generous for an infrequently-used admin API; commands are processed
/// in the main driver loop on every iteration so the channel rarely fills.
pub const ADMIN_CHANNEL_CAPACITY: usize = 32;

/// Runtime state snapshot returned by [`AdminCommand::GetStatus`].
///
/// Serialised directly as the `admin_getBatcherStatus` JSON-RPC response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BatcherStatus {
    /// Whether block ingestion is currently stopped (via admin or the `--stopped` flag).
    pub stopped: bool,
    /// Number of L1 transactions submitted but not yet confirmed.
    pub in_flight: usize,
    /// Estimated unsubmitted DA backlog in bytes.
    pub da_backlog_bytes: u64,
}

/// Errors produced by admin operations.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    /// The driver task has exited and the command channel is closed.
    #[error("admin channel closed: driver has shut down")]
    ChannelClosed,
    /// The requested operation is not yet supported.
    #[error("not yet supported: {0}")]
    NotSupported(&'static str),
    /// The operation needs a running batcher, but it is stopped.
    #[error("batcher is stopped")]
    Stopped,
}

/// Result type alias for admin operations.
pub type AdminResult<T> = Result<T, AdminError>;

/// Commands the admin HTTP server can send to the running driver task.
#[derive(derive_more::Debug)]
pub enum AdminCommand {
    /// Start block ingestion again after a [`Stop`](Self::Stop).
    Start {
        /// Answered once the driver has applied the command.
        #[debug(skip)]
        reply: oneshot::Sender<AdminResult<()>>,
    },
    /// Stop block ingestion; the driver task keeps running.
    Stop {
        /// Answered once the driver has applied the command.
        #[debug(skip)]
        reply: oneshot::Sender<AdminResult<()>>,
    },
    /// Flush the current encoding channel.
    Flush {
        /// Answered once the pipeline is flushed, or with an error if the batcher is stopped.
        #[debug(skip)]
        reply: oneshot::Sender<AdminResult<()>>,
    },
    /// Replace the throttle strategy and configuration.
    SetThrottle {
        /// The new throttle strategy to apply.
        strategy: ThrottleStrategy,
        /// The new throttle configuration to apply.
        #[debug(skip)]
        config: ThrottleConfig,
    },
    /// Clear the throttle dedup cache so limits are re-applied unconditionally.
    ResetThrottle,
    /// Read current throttle state; reply sent via the embedded oneshot sender.
    GetThrottleInfo {
        /// Channel to send the throttle info snapshot back on.
        #[debug(skip)]
        reply: oneshot::Sender<ThrottleInfo>,
    },
    /// Read current driver runtime state; reply sent via the embedded oneshot sender.
    GetStatus {
        /// Channel to send the batcher status back on.
        #[debug(skip)]
        reply: oneshot::Sender<BatcherStatus>,
    },
}

/// Cloneable handle to the driver's admin command channel.
///
/// Create with [`AdminHandle::channel`]; wire the returned
/// [`mpsc::Receiver`] into the driver via [`BatchDriver::with_admin_rx`].
#[derive(Clone, Debug)]
pub struct AdminHandle {
    tx: mpsc::Sender<AdminCommand>,
}

impl AdminHandle {
    /// Create a matched `(AdminHandle, Receiver)` pair.
    pub fn channel() -> (Self, mpsc::Receiver<AdminCommand>) {
        let (tx, rx) = mpsc::channel(ADMIN_CHANNEL_CAPACITY);
        (Self { tx }, rx)
    }

    /// Start block ingestion again. Does nothing if the batcher is already running.
    pub async fn start(&self) -> AdminResult<()> {
        self.request(|reply| AdminCommand::Start { reply }).await
    }

    /// Stop block ingestion; the driver task keeps running.
    ///
    /// In-flight submissions continue to resolve; no new blocks are ingested
    /// until [`start`](Self::start) is called. Does nothing if the batcher is already stopped.
    pub async fn stop(&self) -> AdminResult<()> {
        self.request(|reply| AdminCommand::Stop { reply }).await
    }

    /// Flush the current encoding channel, making its frames eligible for submission.
    ///
    /// Returns [`AdminError::Stopped`] if the batcher is stopped. It does not wait for L1
    /// inclusion.
    pub async fn flush(&self) -> AdminResult<()> {
        self.request(|reply| AdminCommand::Flush { reply }).await
    }

    /// Replace the throttle strategy and configuration.
    ///
    /// The full [`ThrottleConfig`] is required — partial updates are not
    /// supported. Callers that want to change only one field should call
    /// [`get_throttle_info`](Self::get_throttle_info) first to read the
    /// current config, adjust the desired field, and pass the result here.
    pub async fn set_throttle(
        &self,
        strategy: ThrottleStrategy,
        config: ThrottleConfig,
    ) -> AdminResult<()> {
        self.send(AdminCommand::SetThrottle { strategy, config }).await
    }

    /// Clear the throttle dedup cache so limits are re-applied unconditionally
    /// on the next driver iteration.
    pub async fn reset_throttle(&self) -> AdminResult<()> {
        self.send(AdminCommand::ResetThrottle).await
    }

    /// Read the current throttle controller state.
    pub async fn get_throttle_info(&self) -> AdminResult<ThrottleInfo> {
        let (tx, rx) = oneshot::channel();
        self.send(AdminCommand::GetThrottleInfo { reply: tx }).await?;
        rx.await.map_err(|_| AdminError::ChannelClosed)
    }

    /// Read the current driver runtime state.
    pub async fn get_status(&self) -> AdminResult<BatcherStatus> {
        let (tx, rx) = oneshot::channel();
        self.send(AdminCommand::GetStatus { reply: tx }).await?;
        rx.await.map_err(|_| AdminError::ChannelClosed)
    }

    /// Dynamic log level changes require a `tracing-subscriber` reload handle
    /// threaded from the CLI through `BatcherService`.
    ///
    /// Returns an error immediately so callers know the level was not changed.
    /// No command is ever sent to the driver. A future chunk implements this
    /// by modifying `base-cli-utils` to expose a reload handle.
    pub fn set_log_level(&self, _level: String) -> AdminResult<()> {
        Err(AdminError::NotSupported("set_log_level"))
    }

    async fn send(&self, cmd: AdminCommand) -> AdminResult<()> {
        self.tx.send(cmd).await.map_err(|_| AdminError::ChannelClosed)
    }

    /// Send a command carrying a reply channel and wait for the driver's answer.
    async fn request(
        &self,
        command: impl FnOnce(oneshot::Sender<AdminResult<()>>) -> AdminCommand,
    ) -> AdminResult<()> {
        let (reply, rx) = oneshot::channel();
        self.send(command(reply)).await?;
        rx.await.map_err(|_| AdminError::ChannelClosed)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_returns_channel_closed_when_rx_dropped() {
        let (handle, rx) = AdminHandle::channel();
        drop(rx);
        let err = handle.start().await.unwrap_err();
        assert!(matches!(err, AdminError::ChannelClosed));
    }

    #[tokio::test]
    async fn get_status_returns_channel_closed_when_rx_dropped() {
        let (handle, rx) = AdminHandle::channel();
        drop(rx);
        let err = handle.get_status().await.unwrap_err();
        assert!(matches!(err, AdminError::ChannelClosed));
    }

    #[test]
    fn set_log_level_returns_not_supported() {
        let (handle, _rx) = AdminHandle::channel();
        let err = handle.set_log_level("debug".to_string()).unwrap_err();
        assert!(matches!(err, AdminError::NotSupported(_)));
    }
}
