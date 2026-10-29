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
