Admin JSON-RPC API server for the Base Batcher.

Every method except `admin_setLogLevel` answers once the driver has applied it, not when the
command is queued.

| Method | Behavior |
|---|---|
| `admin_stopBatcher` | Stops block ingestion and drops buffered encoding state. Submissions already in flight keep settling; `admin_getBatcherStatus` reports how many remain. Does nothing if the batcher is already stopped. |
| `admin_startBatcher` | Starts ingestion again from the safe L2 head. Does nothing if the batcher is already running. |
| `admin_flushBatcher` | Closes the current channel so its frames become eligible for submission; it does not wait for L1 inclusion. Fails if the batcher is stopped. |
| `admin_getBatcherStatus` | Returns `stopped`, `in_flight` and `da_backlog_bytes`. |
| `admin_setThrottleController` | Replaces the DA throttle strategy and its full configuration. The new limits are pushed to the block builder right after. Fails with `-32602` if the configuration is invalid. |
| `admin_resetThrottleController` | Pushes the current DA limits to the block builder again, even if they have not changed. |
| `admin_getThrottleController` | Returns the throttle strategy, its threshold and maximum intensity, and the current intensity and limits. |
| `admin_setLogLevel` | Not supported yet; always fails with `-32601`. |

Error codes: `-32001` the driver has shut down, `-32002` the batcher is in the wrong state for the
request, `-32601` the method is not supported, `-32602` the parameters are invalid.
