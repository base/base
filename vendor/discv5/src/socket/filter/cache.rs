//! This provides a cache to check rate limits as well as store data for metrics.
//!
//! The cache essentially consists of a time-ordered list of elements. The list is split into two
//! sections, within the enforced time and without.
//!
//!                
//! |               | Enforced Time |  
//! \[x,x,x,x,x,x,x,x,x,x,x,x,x,x,x,x\]
//!                 
//! The enforced time represents one seconds worth of elements. The target aims to limit the
//! number of elements that can be inserted within the enforced time. The length of the list is
//! determined by the `time_window` this can be longer than one second and can be used by metrics
//! to average results over large values than one second.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// The time window that the size of the cache is enforced for. I.e if the size is 5 and
/// ENFORCED_SIZE_TIME is 1 second, this will allow 5 entries per second. This MUST be less than the
/// cache's `time_window`.
pub const ENFORCED_SIZE_TIME: u64 = 1;

pub struct ReceivedPacketCache {
    /// The target number of entries per ENFORCED_SIZE_TIME before inserting new elements reports
    /// failure. The maximum size of the cache is target*time_window
    target: usize,
    /// The cache stores `time_window` seconds worth of information to calculate a moving average.
    /// This variable keeps track the number of elements in the cache within the
    /// ENFORCED_SIZE_TIME.
    time_window: u64,
    /// This stores the current number of messages that are within the `ENFORCED_SIZE_TIME`.
    within_enforced_time: usize,
    /// The underlying data structure. It stores the time when a packet was received.
    inner: VecDeque<Instant>,
}

impl ReceivedPacketCache {
    /// Creates a new `ReceivedPacketCache` with a specified size from which no more can enter.
    pub fn new(target: usize, time_window: u64) -> Self {
        Self {
            target,
            time_window,
            within_enforced_time: 0,
            inner: VecDeque::with_capacity(target * time_window as usize),
        }
    }

    /// Remove expired packets. We only keep, `CACHE_TIME` of data in the cache.
    pub fn reset(&mut self) {
        while let Some(received_at) = self.inner.pop_front() {
            if received_at
                > Instant::now().checked_sub(Duration::from_secs(self.time_window)).unwrap()
            {
                // add the packet back and end
                self.inner.push_front(received_at);
                break;
            }
        }
        // update the within_enforced_time
        let mut count = 0;
        for received_at in self.inner.iter().rev() {
            if *received_at
                > Instant::now().checked_sub(Duration::from_secs(ENFORCED_SIZE_TIME)).unwrap()
            {
                count += 1;
            } else {
                break;
            }
        }
        self.within_enforced_time = count;
    }

    /// Inserts an element into the cache, removing any expired elements.
    pub fn cache_insert(&mut self) -> bool {
        self.reset();
        self.internal_insert()
    }

    /// Inserts an element into the cache without removing expired elements.
    fn internal_insert(&mut self) -> bool {
        if self.within_enforced_time >= self.target {
            // Reached the target
            false
        } else {
            self.inner.push_back(Instant::now());
            self.within_enforced_time += 1;
            true
        }
    }
}

impl std::ops::Deref for ReceivedPacketCache {
    type Target = VecDeque<Instant>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for ReceivedPacketCache {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
