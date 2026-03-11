use base_protocol::MAX_FRAME_LEN;

/// Configuration for [`ChannelDriver`](crate::ChannelDriver).
#[derive(Debug, Clone)]
pub struct ChannelDriverConfig {
    /// Maximum number of bytes per output frame.
    ///
    /// Defaults to [`MAX_FRAME_LEN`] (1 MB). Action tests always produce
    /// batches small enough to fit in a single frame at this size. A smaller
    /// value can be used to force multi-frame output in future tests.
    pub max_frame_size: usize,
}

impl Default for ChannelDriverConfig {
    fn default() -> Self {
        Self { max_frame_size: MAX_FRAME_LEN }
    }
}
