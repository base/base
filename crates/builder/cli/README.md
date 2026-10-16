# Base Builder CLI

Reusable CLI arguments and config conversion helpers for Base builder nodes.

## Validity metadata

`--builder.validity-metadata.path <file>` (env `BASE_BUILDER_VALIDITY_METADATA_PATH`) enables
builder-local validity metadata for `base_getValidityTransactionStatus`. `Args::validity_metadata`
returns a `ValidityMetadataService`: one redb store and one recorder shared, within the
process, by the native payload builder and the status RPC. `base-builder` and `base sequencer`
always register the read-only method, so with the path unset every canonical transaction reports
error `-32050` (`not_recorded`) instead of a classification.

- **Coverage:** only blocks built from the pool by the basic payload builder
  (`--builder.payload-builder-cutover` at and after Denim, or `--builder.basic-payload-builder`)
  are recorded. Flashblocks builds, derived-attribute re-execution (`no_tx_pool`), witness
  generation, cancelled or aborted builds, and blocks imported from another producer are never
  recorded. A warning is logged when the path is set without a basic-builder mode.
- **Failures:** metadata never stops the node. If the store cannot be opened, metadata is disabled
  and an error is logged. If the writer thread cannot start, existing records stay readable and new
  blocks are not recorded. Queue overflow and write failures are counted in
  `builder.validity_metadata.*` metrics and sampled in logs.
- **Lifecycle:** the store is opened during startup, before block production; redb may spend time
  checking or repairing the file after an unclean shutdown. The writer thread is detached, commits
  batches durably, and exits when the recorder is dropped. Records still queued when the process
  exits are lost; those blocks report `not_recorded`, never `false`. Restarting with the same path
  reopens the durable records.
- **Retention:** records are never pruned and the file grows with every recorded block, including
  candidates that are never selected: pre-Denim basic-only payloads that a later rebuild
  improves on, and basic-builder payloads that cutover mode builds alongside Flashblocks before
  Denim. Such records are never served because their blocks are not canonical. Size the disk and
  back the file up. Only one process may open the file.
- **History prerequisites:** lookups use the transaction-hash index and block body indices. A
  warning is logged when node pruning removes either; pruned transactions return `null`.
- **Routing and HA:** the method is merged into the node's configured RPC transports alongside the
  other `base_*` methods. Do not expose the producer's RPC publicly to serve it; route only
  `base_getValidityTransactionStatus` to the producer that built the block. Each producer answers
  only for blocks it built while recording was enabled. Records are not replicated between
  sequencers, so after an HA leadership change, blocks built by another producer report
  `not_recorded` on this node.
