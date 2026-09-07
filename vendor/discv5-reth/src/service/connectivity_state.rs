//! This keeps track of our whether we should advertise an external IP address or not based on
//! whether we think we are externally contactable or not.
//!
//! We determine this by advertising our discovered IP address, if we receive inbound connections,
//! then we know we are externally contactable. If we see nothing for a period of time, we consider
//! ourselves non-contactable and revoke our advertised IP address. We wait for
//! DURATION_UNTIL_NEXT_CONNECTIVITY_ATTEMPT before trying again.
//!
//!
//! The process works via the following:
//! 1. Our ENR socket gets updated
//! 2. This triggers us to set an incoming wait timer
//! 3. a. If we receive an incoming connection within this time, we consider ourselves contactable
//!    and we remove the timer.
//! 3. b. If we don't receive a connection and the timer expires. If the timer expires, we set our
//!    external ENR address to None and set the `next_connectivity_test` to
//!    DURATION_UNTIL_NEXT_CONNECTIVITY_ATTEMPT in the future. This will prevent counting votes until
//!    this time, which prevents our ENR from being updated.

use std::{
    net::SocketAddr,
    pin::Pin,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use futures::{
    FutureExt,
    future::{Either, pending},
};
use tokio::time::{Sleep, sleep};
use tracing::info;

use crate::metrics::METRICS;

pub const DURATION_UNTIL_NEXT_CONNECTIVITY_ATTEMPT: Duration = Duration::from_secs(21600); // 6 hours

/// The number of incoming connections we need to observe before we consider ourselves contactable.
/// A previously disconnected node reconnecting back through a temporarily open port can be false
/// positive. The higher this number, the lower the probability of false positives.
const NUMBER_OF_INCOMING_CONNECTIONS_REQUIRED_TO_BE_VALID: usize = 2;

/// The error returned from polling the ConnectivityState indicating whether IPv4 or IPv6 has
/// failed a connectivity check.
pub enum TimerFailure {
    /// IPv4 Timer failure
    V4,
    /// IPv6 Timer failure
    V6,
}

pub(crate) struct ConnectivityState {
    /// The duration we will wait for incoming connections before deciding if we are contactable or
    /// not. If this is None, we consider ourselves always contactable.
    duration_for_incoming_connections: Option<Duration>,
    /// If we are awaiting for incoming connections, this is the instant that we stop waiting.
    ipv4_incoming_wait_time: Option<Pin<Box<Sleep>>>,
    /// If we are awaiting for incoming connections, this is the instant that we stop waiting.
    ipv6_incoming_wait_time: Option<Pin<Box<Sleep>>>,
    /// The time that we begin checking connectivity tests for ipv4.
    pub ipv4_next_connectivity_test: Instant,
    /// The time that we begin checking connectivity tests for ipv6.
    pub ipv6_next_connectivity_test: Instant,
    /// The number of incoming ipv4 nodes we have seen during our awaiting window.
    ipv4_incoming_count: usize,
    /// The number of incoming ipv6 nodes we have seen during our awaiting window.
    ipv6_incoming_count: usize,
}

impl ConnectivityState {
    pub fn new(duration_for_incoming_connections: Option<Duration>) -> Self {
        ConnectivityState {
            duration_for_incoming_connections,
            ipv4_incoming_wait_time: None,
            ipv6_incoming_wait_time: None,
            ipv4_next_connectivity_test: Instant::now(),
            ipv6_next_connectivity_test: Instant::now(),
            ipv4_incoming_count: 0,
            ipv6_incoming_count: 0,
        }
    }

    /// Checks if we are in a state to handle new IP votes. If we are waiting to do a connectivity
    /// test for this specific ip kind, this returns false.
    pub fn should_count_ip_vote(&self, socket: &SocketAddr) -> bool {
        // If this configuration is not set, we just accept all votes and disable this
        // functionality.
        if self.duration_for_incoming_connections.is_none() {
            return true;
        }

        // If we have failed a connectivity test, then we wait until the next duration window
        // before counting new votes.
        match socket {
            SocketAddr::V4(_) => Instant::now() >= self.ipv4_next_connectivity_test,
            SocketAddr::V6(_) => Instant::now() >= self.ipv6_next_connectivity_test,
        }
    }

    /// We have updated our external ENR socket. If enabled (i.e duration_for_incoming_connections
    /// is not None) then we start a timer to await for any kind of incoming connection. This will
    /// verify that we are contactable. If we receive nothing in `duration_for_incoming_connections` then we consider ourselves non-contactable
    pub fn enr_socket_update(&mut self, socket: &SocketAddr) {
        if let Some(duration_to_wait) = self.duration_for_incoming_connections {
            match socket {
                SocketAddr::V4(_) => {
                    self.ipv4_incoming_count = 0;
                    self.ipv4_incoming_wait_time = Some(Box::pin(sleep(duration_to_wait)))
                }
                SocketAddr::V6(_) => {
                    self.ipv6_incoming_count = 0;
                    self.ipv6_incoming_wait_time = Some(Box::pin(sleep(duration_to_wait)))
                }
            }
        }
    }

    // We have received an incoming connection. If we were awaiting for a connection, we remove the
    // expiry timer and we are done. The ENR will remain advertised and new votes will still count
    // to potentially change the IP address if a legitimate change occurs.
    pub fn received_incoming_connection(&mut self, socket: &SocketAddr) {
        match socket {
            SocketAddr::V4(_) => {
                if self.ipv4_incoming_wait_time.is_none() {
                    // We are not waiting for any v4 connections
                    return;
                }
                self.ipv4_incoming_count += 1;
                if self.ipv4_incoming_count >= NUMBER_OF_INCOMING_CONNECTIONS_REQUIRED_TO_BE_VALID {
                    info!(ip_version = "v4", "We are contactable");
                    self.ipv4_incoming_wait_time = None;
                    METRICS.ipv4_contactable.store(true, Ordering::Relaxed);
                }
            }
            SocketAddr::V6(_) => {
                if self.ipv6_incoming_wait_time.is_none() {
                    // We are not waiting for any v6 connections
                    return;
                }
                self.ipv6_incoming_count += 1;
                if self.ipv6_incoming_count >= NUMBER_OF_INCOMING_CONNECTIONS_REQUIRED_TO_BE_VALID {
                    info!(ip_version = "v6", "We are contactable");
                    self.ipv6_incoming_wait_time = None;
                    METRICS.ipv6_contactable.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    pub async fn poll(&mut self) -> TimerFailure {
        let ipv4_fired =
            match (self.ipv4_incoming_wait_time.as_mut(), self.ipv6_incoming_wait_time.as_mut()) {
                (Some(ipv4_sleep), Some(ipv6_sleep)) => {
                    match futures::future::select(ipv4_sleep, ipv6_sleep).await {
                        Either::Left(_) => true,
                        Either::Right(_) => false, // Ipv6 fired,
                    }
                }
                (Some(ipv4_sleep), None) => ipv4_sleep.map(|_| true).await,
                (None, Some(ipv6_sleep)) => ipv6_sleep.map(|_| false).await,
                (None, None) => pending().await,
            };

        if ipv4_fired {
            self.ipv4_next_connectivity_test =
                Instant::now() + DURATION_UNTIL_NEXT_CONNECTIVITY_ATTEMPT;
            self.ipv4_incoming_wait_time = None;
            METRICS.ipv4_contactable.store(false, Ordering::Relaxed);
            TimerFailure::V4
        } else {
            // Ipv6 fired
            self.ipv6_next_connectivity_test =
                Instant::now() + DURATION_UNTIL_NEXT_CONNECTIVITY_ATTEMPT;
            self.ipv6_incoming_wait_time = None;
            METRICS.ipv6_contactable.store(false, Ordering::Relaxed);
            TimerFailure::V6
        }
    }
}
