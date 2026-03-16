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
`tokio_util::sync::CancellationToken`. It supports child cancellation tokens so that
individual pipeline stages can be shut down independently while a parent runtime remains
live. The test implementation uses a fully custom async executor — no tokio involvement —
with a seeded RNG that shuffles the ready-task queue before each polling round. The same
seed always produces the same task polling order, making races and timing bugs
reproducible. Virtual time only advances when the executor has no ready tasks, jumping
directly to the next alarm deadline so a 100-second interval test completes in
microseconds. The entry point is `Runner::start`; the runtime handle passed to tasks is
`Context`, which implements all three traits.

The `tokio::select!` macro is fully compatible with this abstraction because it generates
standard `std::task::Poll` code rather than calling into any tokio-specific executor API.
Any future returned by `Clock::sleep`, `Clock::interval`, or `Cancellation::cancelled`
can appear as a `select!` arm without modification.

## Usage

### Production

```rust
use base_runtime::TokioRuntime;

let rt = TokioRuntime::new();
// Pass rt into BatchDriver::new(...) or HybridBlockSource::new(...)
```

### Deterministic tests

```rust,ignore
use base_runtime::{Config, Runner, Clock, Spawner};
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
