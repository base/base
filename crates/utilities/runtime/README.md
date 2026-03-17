# base-runtime

base-runtime provides an async runtime abstraction for deterministic testing of base
components such as the batch submission pipeline and the hybrid block source. The crate
defines three composable traits — `Clock`, `Spawner`, and `Cancellation` — and a blanket
`Runtime` supertrait that combines all three. Components accept a single `R: Runtime`
bound rather than concrete tokio types, which makes them testable without wall-clock time
or real concurrency.

This design was inspired by the [commonware](https://github.com/commonwarexyz/monorepo)
project's [commonware-runtime](https://crates.io/crates/commonware-runtime) crate, which
uses a `Runner`/`Context` abstraction to decouple application logic from the underlying
executor. Commonware ships two implementations — a production tokio-backed runtime and a
deterministic simulator — allowing the same application code to be driven with full
control over scheduling and time. base-runtime follows the same pattern but is scoped
to the capabilities the base batch driver and derivation pipeline actually require:
virtual time, task spawning, and structured cancellation.

The production implementation, `TokioRuntime`, wraps `tokio::time`, `tokio::spawn`, and
`tokio_util::sync::CancellationToken`. It supports child cancellation tokens via
`Cancellation::child()` so that individual pipeline stages can be shut down
independently while a parent runtime remains live. Call `TokioRuntime::new()` to create
a fresh cancellation scope, or `TokioRuntime::with_token(token)` to wrap an existing
`CancellationToken` — useful when migrating code that already holds a token into the
`R: Runtime` abstraction. The test implementation uses a fully custom async executor —
no tokio involvement — with a seeded RNG that shuffles the ready-task queue before each
polling round. The same seed always produces the same task polling order, making races
and timing bugs reproducible. Virtual time only advances when the executor has no ready
tasks, jumping directly to the next alarm deadline so a 100-second interval test
completes in microseconds. The entry point is `Runner::start`; the runtime handle passed
to tasks is `Context`, which implements all three traits.

The `tokio::select!` macro is fully compatible with this abstraction because it generates
standard `std::task::Poll` code rather than calling into any tokio-specific executor API.
Any future returned by `Clock::sleep`, `Clock::interval`, or `Cancellation::cancelled`
can appear as a `select!` arm without modification.

Components in this workspace that accept `R: Runtime`:

- `BatchDriver` (`base-batcher-core`) — uses `runtime.cancelled()` as a shutdown signal
  and `runtime.sleep(drain_timeout)` to bound the drain phase after cancellation.
- `HybridBlockSource` (`base-batcher-source`) — uses `runtime.interval(poll_interval)`
  to schedule periodic RPC polling alongside a live subscription stream.

## Usage

### Production

Create a fresh runtime with its own cancellation scope:

```rust
use base_runtime::TokioRuntime;

let rt = TokioRuntime::new();
// Pass rt into BatchDriver::new(...) or HybridBlockSource::new(...)
```

When migrating code that already holds a `CancellationToken`, wrap it instead of
creating a second cancellation scope:

```rust
use base_runtime::TokioRuntime;
use tokio_util::sync::CancellationToken;

let token = CancellationToken::new();
let rt = TokioRuntime::with_token(token.clone());
// Cancelling `token` cancels `rt`, and `rt.cancel()` cancels `token`.
```

To shut down a sub-component independently without affecting its parent, use a child
runtime whose cancellation propagates from parent to child but not in reverse:

```rust
use base_runtime::{TokioRuntime, Cancellation};

let parent = TokioRuntime::new();
let child = parent.child(); // cancelled when parent is cancelled
child.cancel();             // does not cancel parent
```

### Bounded async loops

Components that need to exit after a timeout should drive the deadline through the
runtime rather than calling `tokio::time::sleep` directly. This ensures the same code
path is exercised under the deterministic executor in tests:

```rust,ignore
use base_runtime::{Runtime, Clock};
use std::time::Duration;

async fn drain_with_timeout<R: Runtime>(runtime: R, timeout: Duration) {
    tokio::select! {
        _ = runtime.sleep(timeout) => { /* timed out */ }
        _ = wait_for_confirmations() => { /* done */ }
    }
}
```

### Deterministic tests

```rust,ignore
use base_runtime::deterministic::{Config, Runner};
use base_runtime::{Clock, Spawner};
use std::time::Duration;

#[test]
fn test_timer_fires() {
    Runner::start(Config::seeded(42), |ctx| async move {
        let ctx2 = ctx.clone();
        let handle = ctx.spawn(async move {
            ctx2.sleep(Duration::from_secs(5)).await;
            99u32
        });
        // Executor skips idle time to t=5s, wakes the spawned sleep, then
        // resolves the handle. No wall-clock time is consumed.
        let result = handle.await.unwrap();
        assert_eq!(result, 99);
        assert_eq!(ctx.now(), Duration::from_secs(5));
    });
}
```
