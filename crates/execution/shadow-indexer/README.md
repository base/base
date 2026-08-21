# `base-shadow-indexer`

Shadow indexer Execution Extension (`ExEx`) that captures reorged-out and reverted execution
blocks and persists their metadata to the shadow indexer database. Canonical blocks are not
persisted: only blocks the chain discarded carry shadow-block signal.
