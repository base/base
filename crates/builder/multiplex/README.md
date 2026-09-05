# `base-builder-multiplex`

Runs Base Flashblocks and native (basic) payload builders behind a single routing
`PayloadBuilderHandle`. Flashblocks is eligible through Beryl; any active upgrade after Beryl
selects the native builder.

## Overview

- starts both services by default, regardless of the current or scheduled fork,
- before the first post-Beryl upgrade, selects Flashblocks and runs the native builder as a
  `no_tx_pool` shadow,
- at and after activation, sends build requests only to the native builder: the Flashblocks
  service stays running but does not build or publish post-Beryl payloads,
- routes reads (`BestPayload`, `PayloadTimestamp`, `Resolve`, `Subscribe`) to the builder
  selected for each payload,
- with basic-only mode enabled, starts only the native payload builder.

## Startup configuration

No cutover flag or restart-time configuration change is required. The legacy
`--builder.payload-builder-cutover` flag is accepted but has no effect.

Routing checks the effective fork schedule for each payload timestamp, including runtime upgrade
signals. All upgrades ordered after Beryl are considered, even if Cobalt itself is unscheduled.
Already-created payloads retain their recorded route. This changes builder eligibility, not the
consensus block-time schedule.
