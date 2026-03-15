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
live. The test implementation, `DeterministicRuntime`, is also backed by tokio but uses
tokio's paused-clock mode, requiring `#[tokio::test(start_paused = true)]`. Virtual time
only advances when `advance_time` is called explicitly, so timer-driven logic — polling
intervals, channel timeouts, backoff delays — can be exercised at precise granularity in
tests that complete in real milliseconds. Cancellation in `DeterministicRuntime` uses
`tokio::sync::watch` rather than `CancellationToken`, which allows the signal to remain
valid even when no receivers are subscribed at the moment `cancel` is called.

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

```rust
use base_runtime::DeterministicRuntime;
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn test_timer_fires_on_advance() {
    let rt = DeterministicRuntime::new();
    let rt2 = rt.clone();

    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        rt2.sleep(Duration::from_secs(5)).await;
        let _ = tx.send(());
    });

    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err(), "timer must not fire without advance");

    rt.advance_time(Duration::from_secs(5)).await;
    assert!(rx.try_recv().is_ok(), "timer must fire after advance");
}
```
