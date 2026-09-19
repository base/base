Admin JSON-RPC API server for the Base Batcher.

Lifecycle methods answer once the driver has applied them, not when the command is queued.

| Method | Behavior |
|---|---|
| `admin_stopBatcher` | Stops block ingestion and drops buffered encoding state. Returns once no submission is in flight. If some still are after the driver's drain timeout, returns an error; the batcher stays stopped. Stopping a stopped batcher succeeds. |
| `admin_startBatcher` | Starts ingestion again from the safe L2 head. Does nothing if the batcher is already running. If a stop request is still waiting for in-flight submissions when the start arrives, that stop request returns an error. |
| `admin_flushBatcher` | Closes the current channel and returns the outcome of that flush; it does not wait for L1 inclusion. Fails if the batcher is stopped. A flush failure is fatal for the driver. |
| `admin_getBatcherStatus` | Returns `stopped`, `in_flight` and `da_backlog_bytes`. Always answered, including while a stop is waiting. |

Error codes: `-32001` the driver has shut down, `-32002` the batcher is in the wrong state for the
request, `-32003` the operation failed.
