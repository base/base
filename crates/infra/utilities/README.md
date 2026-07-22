# `base-http-utils`

Shared HTTP utilities for Base services.

## Trusted proxy client IP resolution

Provides [`TrustedProxyConfig`], which resolves the real client IP of an HTTP
request. Forwarding headers such as `X-Forwarded-For` are honored only when the
direct peer address falls inside a configured set of trusted proxy CIDRs;
otherwise the peer socket address is used. With no CIDRs configured, forwarding
headers are never honored, which is the correct mode for directly exposed
deployments.

Extraction takes the rightmost comma-separated entry of the last occurrence of
the configured header, matching `axum-client-ip`'s rightmost semantics: proxies
append the real client address after any client-supplied values, so that is
the only entry the trusted proxy vouches for. Strict callers use
[`TrustedProxyConfig::try_client_ip`] and surface extraction failures from
trusted proxies; lenient callers use [`TrustedProxyConfig::client_ip`], which
falls back to the peer address with a warning.

`IPv4`-mapped `IPv6` addresses are canonicalized so `IPv4` CIDRs match peers on
dual-stack listeners and derived rate-limit buckets stay consistent across
address forms.
