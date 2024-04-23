# L2 Chain Derivation

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Architecture](#architecture)
  - [L2 Chain Derivation Pipeline](#l2-chain-derivation-pipeline)
    - [Batch Queue](#batch-queue)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

# Architecture

## L2 Chain Derivation Pipeline

### Batch Queue

With Fjord, the `max_sequencer_drift` parameter becomes a constant of value
`1800` _seconds_, translating to a fixed maximum sequencer drift of 30 minutes.
