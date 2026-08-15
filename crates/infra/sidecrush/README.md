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

A separate 2 s heartbeat emits one `StatsD` counter increment per tick to
Datadog under the `base.blocks` metric namespace, so alerting systems observe a
continuous signal independent of poll cadence.

## Types

- [`BlockProductionHealthChecker`] — the async polling driver.
- [`AlloyEthClient`] — an `EthClient` impl backed by the `alloy` HTTP provider.
- [`HealthcheckMetrics`] — the `StatsD` counter wrapper.
- [`Node`], [`HealthcheckConfig`], [`HeaderSummary`], [`HealthState`] — value
  types describing the polling target, thresholds, block summaries, and current
  state.

Downstream deployment (Kubernetes manifests, image build) is defined by the
`base-sidecrush-bin` binary crate in `bin/sidecrush/`.
