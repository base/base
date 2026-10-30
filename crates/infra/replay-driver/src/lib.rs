#![doc = include_str!("../README.md")]

mod attributes;
pub use attributes::CanonicalAttributes;

mod candidate;
pub use candidate::Candidate;

mod shadow;
pub use shadow::{BuildWait, ShadowDriver, ShadowError};
