# base-clock

Wall-clock time sources for builder transaction replay. This crate has no external dependencies.

`Clock` returns `SystemTime`; `now_unix_millis()` returns whole milliseconds since
the Unix epoch, or `SystemTimeError` for times before the epoch. `SystemClock`
uses the operating system's wall clock.

```rust
use base_clock::{Clock, SystemClock};

let millis = SystemClock.now_unix_millis()?;
assert!(millis > 0);
# Ok::<(), std::time::SystemTimeError>(())
```

`OffsetClock::new(start)` anchors a chosen `SystemTime` to a monotonic `Instant`.
It advances at real speed even if the operating system's wall clock changes;
it cannot pause or accelerate. Clones retain the original anchor. Like
`SystemTime` addition, `now()` panics if the resulting time is outside the
platform's representable range.
