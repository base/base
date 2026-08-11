# `base-builder-multiplex`

Runs Base flashblocks and Base basic payload builders in parallel behind a single routing
`PayloadBuilderHandle`.

## Overview

- with dual mode enabled, fans out every `BuildNewPayload` request to flashblocks (selected)
  and basic (shadow),
- always routes reads (`BestPayload`, `PayloadTimestamp`, `Resolve`, `Subscribe`) to
  flashblocks,
- keeps basic as validation-only shadow output during 200ms cutover migration,
- defaults dual mode to disabled so startup behavior is identical to plain
  `FlashblocksServiceBuilder`.
