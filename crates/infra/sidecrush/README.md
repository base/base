# base-sidecrush

Block-production health-check sidecar library.

`base-sidecrush` polls an execution-layer node's HTTP RPC endpoint on a fixed
interval, computes the age of the latest block relative to wall-clock time, and
classifies the node into one of four health states:

- **Healthy** — latest block age is within the configured grace period.
- **Delayed** — block age is past the grace period but below the unhealthy
  threshold.
- **Unhealthy** — block age is at or above the unhealthy threshold; block
  production is considered stalled.
- **Error** — the RPC call failed or timed out (2 s per fetch).

With sub-second block times, header timestamps (whole seconds) are too coarse to
measure block age. [`HeadSubscriptionHealthChecker`] instead subscribes to
`newHeads` over WebSocket and measures block age as the local time since the last
advancing head arrived. In this mode the grace period and unhealthy threshold must
be larger than the block time, and **Error** means the subscription is down. Lost
subscriptions are reopened with backoff, and the checker resubscribes after 10 s
without a head in case the subscription died silently. A new instance reports
healthy until a head arrives whose timestamp is within the grace period (plus one
second for rounding) of wall-clock time.

A separate heartbeat (2 s by default) emits one `StatsD` counter increment per tick to
Datadog under the `base.blocks` metric namespace, so alerting systems observe a
continuous signal independent of poll cadence.

## Types

- [`BlockProductionHealthChecker`] — the async polling driver.
- [`HeadSubscriptionHealthChecker`] — the `newHeads` subscription driver.
- [`AlloyEthClient`] — an `EthClient` impl backed by the `alloy` HTTP provider.
- [`AlloyWsHeadSubscriber`] — a `HeadSubscriber` impl backed by the `alloy`
  WebSocket provider.
- [`HealthcheckMetrics`] — the `StatsD` counter wrapper.
- [`Node`], [`HealthcheckConfig`], [`HeaderSummary`], [`HealthState`] — value
  types describing the polling target, thresholds, block summaries, and current
  state.

Downstream deployment (Kubernetes manifests, image build) is defined by the
`base-sidecrush-bin` binary crate in `bin/sidecrush/`.
