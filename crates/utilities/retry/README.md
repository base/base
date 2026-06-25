# `base-retry`

Shared retry configuration helpers for Base services.

The crate provides a shared `RetryConfig` wrapper around `backon` exponential
retry builders so callers can share delay normalization, jitter, and bounded or
unbounded retry-limit behavior without duplicating builder setup in each service
crate.
