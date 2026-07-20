# `base-http-utils`

Shared HTTP utilities for Base services.

## Trusted proxy client IP resolution

Provides [`TrustedProxyConfig`], which resolves the real client IP of an HTTP
request. Forwarding headers such as `X-Forwarded-For` are honored only when the
direct peer address falls inside a configured set of trusted proxy CIDRs;
otherwise the peer socket address is used. `IPv4`-mapped `IPv6` addresses are
canonicalized so `IPv4` CIDRs match peers on dual-stack listeners and derived
rate-limit buckets stay consistent across address forms.
