Admin JSON-RPC API server for the Base Batcher.

Lifecycle methods answer once the driver has applied them, not when the command is queued.

| Method | Behavior |
|---|---|
| `admin_stopBatcher` | Stops block ingestion and drops buffered encoding state. Submissions already in flight keep settling; `admin_getBatcherStatus` reports how many remain. Does nothing if the batcher is already stopped. |
| `admin_startBatcher` | Starts ingestion again from the safe L2 head. Does nothing if the batcher is already running. |
| `admin_flushBatcher` | Closes the current channel so its frames become eligible for submission; it does not wait for L1 inclusion. Fails if the batcher is stopped. |
| `admin_getBatcherStatus` | Returns `stopped`, `in_flight` and `da_backlog_bytes`. |

Error codes: `-32001` the driver has shut down, `-32002` the batcher is in the wrong state for the
request.
