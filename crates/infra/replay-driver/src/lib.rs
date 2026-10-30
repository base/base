#![doc = include_str!("../README.md")]

mod attributes;
pub use attributes::CanonicalAttributes;

mod candidate;
pub use candidate::Candidate;

mod injector;
pub use injector::{InjectionError, InjectionSchedule, Injector};

mod diff;
pub use diff::{BlockDiff, CanonicalBlock, DiffError};

mod report;
pub use report::{DiffReport, DiffTotals};

mod shadow;
pub use shadow::{BuildWait, ShadowDriver, ShadowError};
