//! Integration tests of the batcher encoder through its public API, one module per subject.

pub mod common;

mod backlog;
mod channels;
mod ingest;
mod lease;
mod packing;
mod reconcile;
mod replay;
mod roundtrip;
