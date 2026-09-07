use super::rate_limiter::RateLimiter;

#[derive(Debug)]
pub struct FilterConfig {
    /// Whether the packet filter is enabled or not.
    pub enabled: bool,
    /// Set up various rate limits for unsolicited packets. See the
    /// [`crate::RateLimiterBuilder`] for
    /// further details on constructing rate limits. See the [`Default`] implementation for default
    /// values.
    pub rate_limiter: Option<RateLimiter>,
    /// The maximum number of node-ids allowed per IP address before the IP address gets banned.
    /// Having this set to None, disables this feature. Default value is 10.
    pub max_nodes_per_ip: Option<usize>,
    /// The maximum number of nodes that can be banned by a single IP before that IP gets banned.
    /// The default is 5.
    pub max_bans_per_ip: Option<usize>,
}
