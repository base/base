#![allow(clippy::incompatible_msrv)]

use std::cell::RefCell;
pub use std::hint::black_box;

pub use codspeed_criterion_compat::{BatchSize, Criterion};
pub use nybbles::Nibbles;
pub use proptest::{
    collection::vec as arb_vec,
    strategy::{Strategy, ValueTree},
    test_runner::TestRunner,
};
pub use smallvec::SmallVec;

pub const SIZE_NIBBLES: [usize; 4] = [8, 16, 32, 64];
pub const SIZE_BYTES: [usize; 4] = [4, 8, 16, 32];

pub fn bench_unop<U>(
    criterion: &mut Criterion,
    name: &str,
    size: usize,
    f: impl FnMut(Nibbles) -> U,
) {
    bench_arbitrary_with(criterion, &format!("{name}/{size}"), nibbles_strategy(size), f);
}

pub fn bench_binop<U>(
    criterion: &mut Criterion,
    name: &str,
    size: usize,
    mut f: impl FnMut(Nibbles, Nibbles) -> U,
) {
    bench_arbitrary_with(
        criterion,
        &format!("{name}/{size}"),
        (nibbles_strategy(size), nibbles_strategy(size)),
        move |(a, b)| f(a, b),
    );
}

pub fn bench_arbitrary_with<T: Strategy, U>(
    criterion: &mut Criterion,
    name: &str,
    input: T,
    f: impl FnMut(T::Value) -> U,
) {
    let runner = RefCell::new(TestRunner::deterministic());
    let mut setup = mk_setup(&input, &runner);
    let setup2 = mk_setup(&input, &runner);
    let mut f = manual_batch(setup2, f, name);
    criterion.bench_function(name, move |bencher| {
        bencher.iter_batched(&mut setup, &mut f, BatchSize::SmallInput);
    });
}

fn mk_setup<'a, T: Strategy>(
    input: &'a T,
    runner: &'a RefCell<TestRunner>,
) -> impl FnMut() -> T::Value + 'a {
    move || input.new_tree(&mut runner.borrow_mut()).unwrap().current()
}

/// Codspeed does not batch inputs even if `iter_batched` is used, so we have to
/// do it ourselves for operations that would otherwise be too fast to be
/// measured accurately.
#[cfg(codspeed)]
#[inline]
fn manual_batch<T, U>(
    mut setup: impl FnMut() -> T,
    mut f: impl FnMut(T) -> U,
    name: &str,
) -> impl FnMut(T) {
    assert!(
        !needs_drop::<T>(),
        "{name:?}: cannot batch inputs that need to be dropped: {}",
        std::any::type_name::<T>(),
    );
    assert!(
        !needs_drop::<U>(),
        "{name:?}: cannot batch outputs that need to be dropped: {}",
        std::any::type_name::<U>(),
    );

    let batch_size = black_box(10000);
    let inputs = black_box((0..batch_size).map(|_| setup()).collect::<Box<[_]>>());
    let mut out = black_box(Box::new_uninit_slice(batch_size));
    move |_| {
        for i in 0..batch_size {
            let input = unsafe { std::ptr::read(inputs.get_unchecked(i)) };
            let output = unsafe { out.get_unchecked_mut(i) };
            output.write(f(input));
        }
    }
}

#[cfg(codspeed)]
fn needs_drop<T>() -> bool {
    let name = std::any::type_name::<T>();
    // SAFETY: These types don't implement `Copy` when `T: Copy` even though
    // they can.
    if name.contains("SmallVec<") || name.contains("Nibbles") {
        false
    } else {
        std::mem::needs_drop::<T>()
    }
}

#[cfg(not(codspeed))]
fn manual_batch<T, U>(
    _setup: impl FnMut() -> T,
    f: impl FnMut(T) -> U,
    _name: &str,
) -> impl FnMut(T) -> U {
    f
}

pub fn nibbles_strategy(len: usize) -> impl Strategy<Value = Nibbles> {
    arb_vec(0u8..16, len).prop_map(|v| Nibbles::from_nibbles(v))
}

// pub fn bytes_strategy(len: usize) -> impl Strategy<Value = Vec<u8>> {
//     arb_vec(proptest::arbitrary::any::<u8>(), len)
// }
