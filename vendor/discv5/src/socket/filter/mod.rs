//! A filter which decides whether to accept/reject incoming UDP packets.

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use cache::ReceivedPacketCache;
use enr::NodeId;
use hashlink::LruCache;
use tracing::{debug, warn};

use crate::{discv5::PERMIT_BAN_LIST, metrics::METRICS, node_info::NodeAddress, packet::Packet};

mod cache;
mod config;
pub mod rate_limiter;
pub use config::FilterConfig;
use rate_limiter::{LimitKind, RateLimiter};

/// The maximum number of IPs to retain when calculating the number of nodes per IP.
const KNOWN_ADDRS_SIZE: usize = 500;
/// The number of IPs to retain at any given time that have banned nodes.
const BANNED_NODES_SIZE: usize = 50;

/// The maximum number of packets to keep record of for metrics if the rate limiter is not
/// specified.
const DEFAULT_PACKETS_PER_SECOND: usize = 20;

/// The packet filter which decides whether we accept or reject incoming packets.
pub(crate) struct Filter {
    /// Whether the filter is enabled or not.
    enabled: bool,
    /// An optional rate limiter for incoming packets.
    rate_limiter: Option<RateLimiter>,
    /// An ordered (by time) collection of recently seen packets by SocketAddr. The packet data is not
    /// stored here. This stores 5 seconds of history to calculate a 5 second moving average for
    /// the metrics.
    raw_packets_received: ReceivedPacketCache,
    /// The duration that bans by this filter last.
    ban_duration: Option<Duration>,
    /// Keep track of node ids per socket. If someone is using too many node-ids per IP, they can
    /// be banned.
    known_addrs: LruCache<IpAddr, HashSet<NodeId>>,
    /// Keep track of Ips that have banned nodes. If a single IP has many nodes that get banned,
    /// then we ban the IP address.
    banned_nodes: LruCache<IpAddr, usize>,
    /// The maximum number of node-ids allowed per IP address before the IP address gets banned.
    /// Having this set to None, disables this feature. Default value is 10.
    pub max_nodes_per_ip: Option<usize>,
    /// The maximum number of nodes that can be banned by a single IP before that IP gets banned.
    /// The default is 5.
    pub max_bans_per_ip: Option<usize>,
}

impl Filter {
    pub fn new(config: FilterConfig, ban_duration: Option<Duration>) -> Filter {
        let expected_packets_per_second = config
            .rate_limiter
            .as_ref()
            .map(|v| v.total_requests_per_second().round() as usize)
            .unwrap_or(DEFAULT_PACKETS_PER_SECOND);

        Filter {
            enabled: config.enabled,
            rate_limiter: config.rate_limiter,
            raw_packets_received: ReceivedPacketCache::new(
                expected_packets_per_second,
                METRICS.moving_window,
            ),
            known_addrs: LruCache::new(KNOWN_ADDRS_SIZE),
            banned_nodes: LruCache::new(BANNED_NODES_SIZE),
            ban_duration,
            max_nodes_per_ip: config.max_nodes_per_ip,
            max_bans_per_ip: config.max_bans_per_ip,
        }
    }

    /// The first check. This determines if a new UDP packet should be decoded or dropped.
    /// Only unsolicited packets arrive here.
    pub fn initial_pass(&mut self, src: &SocketAddr) -> bool {
        if PERMIT_BAN_LIST.read().permit_ips.contains(&src.ip()) {
            return true;
        }

        if PERMIT_BAN_LIST.read().ban_ips.contains_key(&src.ip()) {
            debug!(?src, "Dropped unsolicited packet from banned src");
            return false;
        }

        // Add the un-solicited request to the cache
        // If this is over the maximum requests per ENFORCED_SIZE_TIME, it will not be added, we
        // leave the rate limiter to enforce the rate limits..
        self.raw_packets_received.cache_insert();

        // build the metrics
        METRICS
            .unsolicited_requests_per_window
            .store(self.raw_packets_received.len(), Ordering::Relaxed);

        // If the filter isn't enabled, pass the packet
        if !self.enabled {
            return true;
        }

        // Check rate limits
        if let Some(rate_limiter) = self.rate_limiter.as_mut() {
            if rate_limiter.allows(&LimitKind::Ip(src.ip())).is_err() {
                warn!(ip = ?src.ip(), "Banning IP for excessive requests");
                // Ban the IP address
                let ban_timeout = self.ban_duration.map(|v| Instant::now() + v);
                PERMIT_BAN_LIST.write().ban_ips.insert(src.ip(), ban_timeout);
                return false;
            }

            if rate_limiter.allows(&LimitKind::Total).is_err() {
                debug!(ip = ?src.ip(), "Dropped unsolicited packet from RPC limit");
                return false;
            }
        }
        true
    }

    pub fn final_pass(&mut self, node_address: &NodeAddress, _packet: &Packet) -> bool {
        if PERMIT_BAN_LIST.read().permit_nodes.contains(&node_address.node_id) {
            return true;
        }

        if PERMIT_BAN_LIST.read().ban_nodes.contains_key(&node_address.node_id) {
            debug!(
                node = %node_address,
                "Dropped unsolicited packet from banned node_id",
            );
            return false;
        }

        // If the filter isn't enabled, just pass the packet.
        if !self.enabled {
            return true;
        }

        if let Some(rate_limiter) = self.rate_limiter.as_mut() {
            if rate_limiter.allows(&LimitKind::NodeId(node_address.node_id)).is_err() {
                warn!(
                    node_id = %node_address.node_id,
                    "Node has exceeded its request limit and is now banned",
                );

                // The node is being banned
                let ban_timeout = self.ban_duration.map(|v| Instant::now() + v);
                PERMIT_BAN_LIST.write().ban_nodes.insert(node_address.node_id, ban_timeout);

                // If we are tracking banned nodes per IP, add to the count. If the count is higher
                // than our tolerance, ban the IP.
                if let Some(max_bans_per_ip) = self.max_bans_per_ip {
                    let ip = node_address.socket_addr.ip();
                    if let Some(banned_count) = self.banned_nodes.get_mut(&ip) {
                        *banned_count += 1;
                        if *banned_count >= max_bans_per_ip {
                            PERMIT_BAN_LIST.write().ban_ips.insert(ip, ban_timeout);
                        }
                    } else {
                        self.banned_nodes.insert(ip, 0);
                    }
                }

                return false;
            }
        }

        // Check the nodes per IP filter configuration
        if let Some(max_nodes_per_ip) = self.max_nodes_per_ip {
            // This option is set, store the known nodes per IP.
            let ip = node_address.socket_addr.ip();
            let known_nodes = {
                if let Some(known_nodes) = self.known_addrs.get_mut(&ip) {
                    known_nodes.insert(node_address.node_id);
                    known_nodes.len()
                } else {
                    let mut ids = HashSet::new();
                    ids.insert(node_address.node_id);
                    self.known_addrs.insert(ip, ids);
                    1
                }
            };

            if known_nodes >= max_nodes_per_ip {
                warn!(%ip, "IP has exceeded its node-id limit and is now banned");
                // The node is being banned
                let ban_timeout = self.ban_duration.map(|v| Instant::now() + v);
                PERMIT_BAN_LIST.write().ban_ips.insert(ip, ban_timeout);
                self.known_addrs.remove(&ip);
                return false;
            }
        }

        true
    }

    pub fn prune_limiter(&mut self) {
        if let Some(rate_limiter) = self.rate_limiter.as_mut() {
            rate_limiter.prune();
        }
    }
}
