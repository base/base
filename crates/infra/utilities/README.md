# `base-http-utils`

Shared HTTP utilities for Base services.

## Trusted proxy client IP resolution

Provides [`TrustedProxyConfig`], which resolves the real client IP of an HTTP
request. Forwarding headers such as `X-Forwarded-For` are honored only when the
direct peer address falls inside a configured set of trusted proxy CIDRs;
otherwise the peer socket address is used. With no CIDRs configured, forwarding
headers are never honored, which is the correct mode for directly exposed
deployments.

All occurrences of the configured header are concatenated in order into a
forwarding chain, then scanned right to left, skipping any address in the
trusted proxy CIDRs. The first untrusted address is the real client; if every
forwarded hop is trusted, resolution falls back to the direct peer. This peels
our own proxy hops (e.g. Cloudflare in front of an ALB) instead of stopping at
the innermost proxy, so clients behind multiple trusted hops resolve individually.
Strict callers use
[`TrustedProxyConfig::try_client_ip`] and surface extraction failures from
trusted proxies; lenient callers use [`TrustedProxyConfig::client_ip`], which
falls back to the peer address with a warning.

`IPv4`-mapped `IPv6` addresses are canonicalized so `IPv4` CIDRs match peers on
dual-stack listeners and derived rate-limit buckets stay consistent across
address forms.
