#![allow(clippy::incompatible_msrv)]
#![allow(unexpected_cfgs)]

use codspeed_criterion_compat::{criterion_group, criterion_main};

pub mod benches;
pub use benches::prelude;

criterion_group!(bench, benches::group);
criterion_main!(bench);
