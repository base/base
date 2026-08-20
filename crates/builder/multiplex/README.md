# `base-builder-multiplex`

Runs Base flashblocks and Base basic payload builders in parallel behind a single routing
`PayloadBuilderHandle`, cutting the selected builder over when Zenith activates.

## Overview

- with cutover mode enabled, fans out every `BuildNewPayload` request to both builders,
- selects flashblocks before Zenith and basic at and after Zenith,
- routes reads (`BestPayload`, `PayloadTimestamp`, `Resolve`, `Subscribe`) to the builder
  selected for each payload,
- defaults cutover mode to disabled so startup behavior is identical to plain
  `FlashblocksServiceBuilder`.
