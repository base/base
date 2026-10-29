#![doc = include_str!("../README.md")]

mod clock;
pub use clock::Clock;

mod system;
pub use system::SystemClock;

mod offset;
pub use offset::OffsetClock;

mod global;
pub use global::{GlobalClock, SetClockError};
