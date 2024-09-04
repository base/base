//! Transaction Types

use crate::Frame;
#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use alloy_primitives::Bytes;

/// BatchTransaction is a set of [Frame]s that can be [Into::into] [Bytes].
/// if the size exceeds the desired threshold.
#[derive(Debug, Clone)]
pub struct BatchTransaction {
    /// The frames in the batch.
    pub frames: Vec<Frame>,
    /// The size of the potential transaction.
    pub size: usize,
}

impl BatchTransaction {
    /// Returns the size of the transaction.
    pub const fn size(&self) -> usize {
        self.size
    }

    /// Returns if the transaction has reached the max frame count.
    pub fn is_full(&self, max_frames: u16) -> bool {
        self.frames.len() as u16 >= max_frames
    }
}

impl From<&BatchTransaction> for Bytes {
    fn from(tx: &BatchTransaction) -> Self {
        let mut buf: Vec<u8> = Vec::new();
        for frame in tx.frames.iter() {
            buf.append(&mut frame.encode());
        }
        buf.into()
    }
}
