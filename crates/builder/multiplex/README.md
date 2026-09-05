# `base-builder-multiplex`

Runs Base flashblocks and Base basic payload builders in parallel behind a single routing
`PayloadBuilderHandle`, cutting the selected builder over when Cobalt activates.

## Overview

- with cutover mode enabled, fans out every `BuildNewPayload` request to both builders,
- selects flashblocks before Cobalt and basic at and after Cobalt,
- routes reads (`BestPayload`, `PayloadTimestamp`, `Resolve`, `Subscribe`) to the builder
  selected for each payload,
- with basic-only mode enabled, starts only the basic payload builder for operation after the
  cutover is complete,
- defaults cutover mode to disabled so startup behavior is identical to plain
  `FlashblocksServiceBuilder`.
