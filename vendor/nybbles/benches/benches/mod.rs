mod clone;
mod cmp;
mod convert;
mod iter;
mod ops;
mod slice;

pub(crate) mod prelude;

pub fn group(c: &mut codspeed_criterion_compat::Criterion) {
    convert::group(c);
    ops::group(c);
    slice::group(c);
    iter::group(c);
    cmp::group(c);
    clone::group(c);
}
