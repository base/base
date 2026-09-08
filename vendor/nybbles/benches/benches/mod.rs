//! Benchmark groups and shared test data.

pub mod clone;
pub mod cmp;
pub mod convert;
pub mod iter;
pub mod ops;
pub mod slice;

pub mod prelude;

pub fn group(c: &mut codspeed_criterion_compat::Criterion) {
    convert::group(c);
    ops::group(c);
    slice::group(c);
    iter::group(c);
    cmp::group(c);
    clone::group(c);
}
