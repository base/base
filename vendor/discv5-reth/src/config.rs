//! A set of configuration parameters to tune the discovery protocol.
use std::time::Duration;

use crate::{
    Enr, Executor, PermitBanList, ProtocolIdentity, RateLimiter, RateLimiterBuilder,
    kbucket::MAX_NODES_PER_BUCKET, socket::ListenConfig,
};

/// Configuration parameters that define the performance of the discovery network.
#[derive(Clone)]
pub struct Config {
    /// Whether to enable the incoming packet filter. Default: false.
    pub enable_packet_filter: bool,

    /// The request timeout for each UDP request. Default: 1 seconds.
    pub request_timeout: Duration,

    /// The interval over which votes are remembered when determining our external IP. A lower
    /// interval will respond faster to IP changes. Default is 2 minutes.
    pub vote_duration: Duration,

    /// The timeout after which a `QueryPeer` in an ongoing query is marked unresponsive.
    /// Unresponsive peers don't count towards the parallelism limits for a query.
    /// Hence, we may potentially end up making more requests to good peers. Default: 2 seconds.
    pub query_peer_timeout: Duration,

    /// The timeout for an entire query. Any peers discovered for this query are returned. Default 60 seconds.
    pub query_timeout: Duration,

    /// The number of retries for each UDP request. Default: 1.
    pub request_retries: u8,

    /// The session timeout for each node. Default: 1 day.
    pub session_timeout: Duration,

    /// The maximum number of established sessions to maintain. Default: 1000.
    pub session_cache_capacity: usize,

    /// Updates the local ENR IP and port based on PONG responses from peers. Default: true.
    pub enr_update: bool,

    /// The maximum number of nodes we return to a find nodes request. The default is 16.
    pub max_nodes_response: usize,

    /// The minimum number of peer's who agree on an external IP port before updating the
    /// local ENR. Default: 10.
    pub enr_peer_update_min: usize,

    /// The number of peers to request in parallel in a single query. Default: 3.
    pub query_parallelism: usize,

    /// Limits the number of IP addresses from the same
    /// /24 subnet in the kbuckets table. This is to mitigate eclipse attacks. Default: false.
    pub ip_limit: bool,

    /// Sets a maximum limit to the number of  incoming nodes (nodes that have dialed us) to exist per-bucket. This cannot be larger
    /// than the bucket size (16). By default this is disabled (set to the maximum bucket size, 16).
    pub incoming_bucket_limit: usize,

    /// A filter used to decide whether to insert nodes into our local routing table. Nodes can be
    /// excluded if they do not pass this filter. The default is to accept all nodes.
    pub table_filter: fn(&Enr) -> bool,

    /// The time between pings to ensure connectivity amongst connected nodes. Default: 300
    /// seconds.
    pub ping_interval: Duration,

    /// Reports all discovered ENR's when traversing the DHT to the event stream. Default true.
    pub report_discovered_peers: bool,

    /// A set of configuration parameters for setting inbound request rate limits. See
    /// [`RateLimiterBuilder`] for options. This is only functional if the packet filter is
    /// enabled via the `enable_packet_filter` option. See the `Default` implementation for
    /// default values. If set to None, inbound requests are not filtered.
    pub filter_rate_limiter: Option<RateLimiter>,

    /// The maximum number of node-ids allowed per IP address before the IP address gets banned.
    /// Having this set to None, disables this feature. Default value is 10. This is only
    /// applicable if the `enable_packet_filter` option is set.
    pub filter_max_nodes_per_ip: Option<usize>,

    /// The maximum number of nodes that can be banned by a single IP before that IP gets banned.
    /// The default is 5. This is only
    /// applicable if the `enable_packet_filter` option is set.
    pub filter_max_bans_per_ip: Option<usize>,

    /// A set of lists that permit or ban IP's or NodeIds from the server. See
    /// `crate::PermitBanList`.
    pub permit_ban_list: PermitBanList,

    /// Set the default duration for which nodes are banned for. This timeouts are checked every 5 minutes,
    /// so the precision will be to the nearest 5 minutes. If set to `None`, bans from the filter
    /// will last indefinitely. Default is 1 hour.
    pub ban_duration: Option<Duration>,

    /// Auto-discovering our IP address, is only one part in discovering our NAT/firewall
    /// situation. We need to determine if we are behind a firewall that is preventing incoming
    /// connections (this is especially true for IPv6 where all connections will report the same
    /// external IP). To do this, Discv5 uses a heuristic, which is that after we set an address in
    /// our ENR, we wait for this duration to see if we have any incoming connections. If we
    /// receive a single INCOMING connection in this duration, we consider ourselves contactable,
    /// until we update or change our IP address again. If we fail to receive an incoming
    /// connection in this duration, we revoke our ENR address advertisement for 6 hours, before
    /// trying again. This can be set to None, to always advertise and never revoke. The default is
    /// Some(5 minutes).
    pub auto_nat_listen_duration: Option<Duration>,

    /// A custom executor which can spawn the discv5 tasks. This must be a tokio runtime, with
    /// timing support. By default, the executor that created the discv5 struct will be used.
    pub executor: Option<Box<dyn Executor + Send + Sync>>,

    /// Configuration for the sockets to listen on.
    pub listen_config: ListenConfig,

    /// The protocol identity to use in network messages.
    pub protocol_identity: ProtocolIdentity,
}

#[derive(Debug)]
pub struct ConfigBuilder {
    config: Config,
}

impl ConfigBuilder {
    pub fn new(listen_config: ListenConfig) -> Self {
        // This is only applicable if enable_packet_filter is set.
        let filter_rate_limiter = Some(
            RateLimiterBuilder::new()
                .total_n_every(10, Duration::from_secs(1)) // Allow bursts, average 10 per second
                .node_n_every(8, Duration::from_secs(1)) // Allow bursts, average 8 per second
                .ip_n_every(9, Duration::from_secs(1)) // Allow bursts, average 9 per second
                .build()
                .expect("The total rate limit has been specified"),
        );

        // set default values
        let config = Config {
            enable_packet_filter: false,
            request_timeout: Duration::from_secs(1),
            vote_duration: Duration::from_secs(120),
            query_peer_timeout: Duration::from_secs(2),
            query_timeout: Duration::from_secs(60),
            request_retries: 1,
            session_timeout: Duration::from_secs(86400),
            session_cache_capacity: 1000,
            enr_update: true,
            max_nodes_response: 16,
            enr_peer_update_min: 10,
            query_parallelism: 3,
            ip_limit: false,
            incoming_bucket_limit: MAX_NODES_PER_BUCKET,
            table_filter: |_| true,
            ping_interval: Duration::from_secs(300),
            report_discovered_peers: true,
            filter_rate_limiter,
            filter_max_nodes_per_ip: Some(10),
            filter_max_bans_per_ip: Some(5),
            permit_ban_list: PermitBanList::default(),
            ban_duration: Some(Duration::from_secs(3600)), // 1 hour
            auto_nat_listen_duration: Some(Duration::from_secs(300)), // 5 minutes
            executor: None,
            listen_config,
            protocol_identity: ProtocolIdentity::default(),
        };

        ConfigBuilder { config }
    }

    /// Whether to enable the incoming packet filter.
    pub fn enable_packet_filter(&mut self) -> &mut Self {
        self.config.enable_packet_filter = true;
        self
    }

    /// The request timeout for each UDP request.
    pub fn request_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.config.request_timeout = timeout;
        self
    }

    /// The interval over which votes are remembered when determining our external IP. A lower
    /// interval will respond faster to IP changes. Default is 2 minutes.
    pub fn vote_duration(&mut self, vote_duration: Duration) -> &mut Self {
        self.config.vote_duration = vote_duration;
        self
    }

    /// The timeout after which a `QueryPeer` in an ongoing query is marked unresponsive.
    /// Unresponsive peers don't count towards the parallelism limits for a query.
    /// Hence, we may potentially end up making more requests to good peers.
    pub fn query_peer_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.config.query_peer_timeout = timeout;
        self
    }

    /// The timeout for an entire query. Any peers discovered before this timeout are returned.
    pub fn query_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.config.query_timeout = timeout;
        self
    }

    /// The number of retries for each UDP request.
    pub fn request_retries(&mut self, retries: u8) -> &mut Self {
        self.config.request_retries = retries;
        self
    }

    /// The session timeout for each node.
    pub fn session_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.config.session_timeout = timeout;
        self
    }

    /// The maximum number of established sessions to maintain.
    pub fn session_cache_capacity(&mut self, capacity: usize) -> &mut Self {
        self.config.session_cache_capacity = capacity;
        self
    }

    /// Disables the auto-update of the local ENR IP and port based on PONG responses from peers.
    pub fn disable_enr_update(&mut self) -> &mut Self {
        self.config.enr_update = false;
        self
    }

    /// The maximum number of nodes we response to a find nodes request.
    pub fn max_nodes_response(&mut self, max: usize) -> &mut Self {
        self.config.max_nodes_response = max;
        self
    }

    /// The minimum number of peer's who agree on an external IP port before updating the
    /// local ENR.
    pub fn enr_peer_update_min(&mut self, min: usize) -> &mut Self {
        if min < 2 {
            panic!(
                "Setting enr_peer_update_min to a value less than 2 will cause issues with discovery with peers behind NAT"
            );
        }
        self.config.enr_peer_update_min = min;
        self
    }

    /// The number of peers to request in parallel in a single query.
    pub fn query_parallelism(&mut self, parallelism: usize) -> &mut Self {
        self.config.query_parallelism = parallelism;
        self
    }

    /// Limits the number of IP addresses from the same
    /// /24 subnet in the kbuckets table. This is to mitigate eclipse attacks.
    pub fn ip_limit(&mut self) -> &mut Self {
        self.config.ip_limit = true;
        self
    }

    /// Sets a maximum limit to the number of  incoming nodes (nodes that have dialed us) to exist per-bucket. This cannot be larger
    /// than the bucket size (16). By default, half of every bucket (8 positions) is the largest number of nodes that we accept that dial us.
    pub fn incoming_bucket_limit(&mut self, limit: usize) -> &mut Self {
        self.config.incoming_bucket_limit = limit;
        self
    }

    /// A filter used to decide whether to insert nodes into our local routing table. Nodes can be
    /// excluded if they do not pass this filter.
    pub fn table_filter(&mut self, filter: fn(&Enr) -> bool) -> &mut Self {
        self.config.table_filter = filter;
        self
    }

    /// The time between pings to ensure connectivity amongst connected nodes.
    pub fn ping_interval(&mut self, interval: Duration) -> &mut Self {
        self.config.ping_interval = interval;
        self
    }

    /// Disables reporting of discovered peers through the event stream.
    pub fn disable_report_discovered_peers(&mut self) -> &mut Self {
        self.config.report_discovered_peers = false;
        self
    }

    /// A rate limiter for limiting inbound requests.
    pub fn filter_rate_limiter(&mut self, rate_limiter: Option<RateLimiter>) -> &mut Self {
        self.config.filter_rate_limiter = rate_limiter;
        self
    }

    /// If the filter is enabled, sets the maximum number of nodes per IP before banning
    /// the IP.
    pub fn filter_max_nodes_per_ip(&mut self, max_nodes_per_ip: Option<usize>) -> &mut Self {
        self.config.filter_max_nodes_per_ip = max_nodes_per_ip;
        self
    }

    /// The maximum number of times nodes from a single IP can be banned, before the IP itself
    /// gets banned.
    pub fn filter_max_bans_per_ip(&mut self, max_bans_per_ip: Option<usize>) -> &mut Self {
        self.config.filter_max_bans_per_ip = max_bans_per_ip;
        self
    }

    /// A set of lists that permit or ban IP's or NodeIds from the server. See
    /// `crate::PermitBanList`.
    pub fn permit_ban_list(&mut self, list: PermitBanList) -> &mut Self {
        self.config.permit_ban_list = list;
        self
    }

    /// Set the default duration for which nodes are banned for. This timeouts are checked every 5 minutes,
    /// so the precision will be to the nearest 5 minutes. If set to `None`, bans from the filter
    /// will last indefinitely. Default is 1 hour.
    pub fn ban_duration(&mut self, ban_duration: Option<Duration>) -> &mut Self {
        self.config.ban_duration = ban_duration;
        self
    }

    /// Auto-discovering our IP address, is only one part in discovering our NAT/firewall
    /// situation. We need to determine if we are behind a firewall that is preventing incoming
    /// connections (this is especially true for IPv6 where all connections will report the same
    /// external IP). To do this, Discv5 uses a heuristic, which is that after we set an address in
    /// our ENR, we wait for this duration to see if we have any incoming connections. If we
    /// receive a single INCOMING connection in this duration, we consider ourselves contactable,
    /// until we update or change our IP address again. If we fail to receive an incoming
    /// connection in this duration, we revoke our ENR address advertisement for 6 hours, before
    /// trying again. This can be set to None, to always advertise and never revoke. The default is
    /// Some(5 minutes).
    pub fn auto_nat_listen_duration(
        &mut self,
        auto_nat_listen_duration: Option<Duration>,
    ) -> &mut Self {
        self.config.auto_nat_listen_duration = auto_nat_listen_duration;
        self
    }

    /// A custom executor which can spawn the discv5 tasks. This must be a tokio runtime, with
    /// timing support.
    pub fn executor(&mut self, executor: Box<dyn Executor + Send + Sync>) -> &mut Self {
        self.config.executor = Some(executor);
        self
    }

    /// Configures use of a custom protocol identifier to use in network messages. Any packets not
    /// matching the configured protocol identity will be ignored.
    pub fn protocol_identity(&mut self, protocol_identity: ProtocolIdentity) -> &mut Self {
        self.config.protocol_identity = protocol_identity;
        self
    }

    pub fn build(&mut self) -> Config {
        // If an executor is not provided, assume a current tokio runtime is running.
        if self.config.executor.is_none() {
            self.config.executor = Some(Box::<crate::executor::TokioExecutor>::default());
        };

        // If enr-update is set to false, then it is non-intuitive for discv5 to revoke ENR details
        // when determining NAT status. So we will not do this.
        if self.config.enr_update {
            self.config.auto_nat_listen_duration = None;
        }

        assert!(self.config.incoming_bucket_limit <= MAX_NODES_PER_BUCKET);

        self.config.clone()
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("filter_enabled", &self.enable_packet_filter)
            .field("request_timeout", &self.request_timeout)
            .field("vote_duration", &self.vote_duration)
            .field("query_timeout", &self.query_timeout)
            .field("query_peer_timeout", &self.query_peer_timeout)
            .field("request_retries", &self.request_retries)
            .field("session_timeout", &self.session_timeout)
            .field("session_cache_capacity", &self.session_cache_capacity)
            .field("enr_update", &self.enr_update)
            .field("query_parallelism", &self.query_parallelism)
            .field("report_discovered_peers", &self.report_discovered_peers)
            .field("ip_limit", &self.ip_limit)
            .field("filter_max_nodes_per_ip", &self.filter_max_nodes_per_ip)
            .field("filter_max_bans_per_ip", &self.filter_max_bans_per_ip)
            .field("ip_limit", &self.ip_limit)
            .field("incoming_bucket_limit", &self.incoming_bucket_limit)
            .field("ping_interval", &self.ping_interval)
            .field("ban_duration", &self.ban_duration)
            .field("listen_config", &self.listen_config)
            .finish()
    }
}
