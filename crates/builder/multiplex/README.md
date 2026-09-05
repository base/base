# `base-builder-multiplex`

Runs Base flashblocks and Base basic payload builders in parallel behind a single routing
`PayloadBuilderHandle`, cutting the selected builder over when Denim activates.

## Overview

- with cutover mode enabled, fans out every `BuildNewPayload` request to both builders,
- selects flashblocks before Denim and basic at and after Denim,
- routes reads (`BestPayload`, `PayloadTimestamp`, `Resolve`, `Subscribe`) to the builder
  selected for each payload,
- with basic-only mode enabled, starts only the basic payload builder for operation after the
  cutover is complete,
- allows the default Flashblocks-only mode only when neither Cobalt nor Denim is scheduled.

## Startup configuration

When Cobalt or Denim is scheduled (including future activation), startup requires
`--builder.payload-builder-cutover` or `--builder.basic-payload-builder`. The check uses the
execution chain spec after startup upgrade signals have been applied and rejects Flashblocks-only
mode before starting a payload service. Cutover mode still selects the basic builder at Denim;
this configuration requirement does not change fork activation behavior.

For chains that receive upgrade schedules at runtime, enable cutover mode before starting the
node even if neither fork is scheduled yet. The startup check does not monitor later schedules.
