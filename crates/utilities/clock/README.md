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

`GlobalClock::get().now()` reads the process-wide clock, falling back to
`SystemClock` until `GlobalClock::set(clock)` installs a replacement. Installation
is set-once, including across threads; subsequent attempts return `SetClockError`
and leave the installed clock unchanged. Reading the default does not install it.
Install before starting replay workers, and fetch the clock on each use rather
than caching the default reference. There is no reset API.

```rust
use std::time::{Duration, SystemTime};
use base_clock::{GlobalClock, OffsetClock, SetClockError, SystemClock};

let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
GlobalClock::set(OffsetClock::new(start))?;
assert!(GlobalClock::get().now() >= start);
assert_eq!(GlobalClock::set(SystemClock), Err(SetClockError));
# Ok::<(), SetClockError>(())
```
