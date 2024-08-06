// This code is taken from Kona.
// Reference: https://github.com/ethereum-optimism/kona/blob/main/crates/common/src/executor.rs
// Copyright © 2023 kona contributors Copyright © 2024 Optimism
// Licensed under the [MIT License](https://github.com/ethereum-optimism/kona/blob/main/LICENSE.md).

//! This module contains utilities for handling async functions in the no_std environment. This
//! allows for usage of async/await syntax for futures in a single thread.

use alloc::boxed::Box;
use core::{
    future::Future,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

/// This function busy waits on a future until it is ready. It uses a no-op waker to poll the future
/// in a thread-blocking loop.
pub fn block_on<T>(f: impl Future<Output = T>) -> T {
    let mut f = Box::pin(f);

    // Construct a no-op waker.
    fn noop_clone(_: *const ()) -> RawWaker {
        noop_raw_waker()
    }
    fn noop(_: *const ()) {}
    fn noop_raw_waker() -> RawWaker {
        let vtable = &RawWakerVTable::new(noop_clone, noop, noop, noop);
        RawWaker::new(core::ptr::null(), vtable)
    }
    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut context = Context::from_waker(&waker);

    loop {
        // Safety: This is safe because we only poll the future once per loop iteration,
        // and we do not move the future after pinning it.
        if let Poll::Ready(v) = f.as_mut().poll(&mut context) {
            return v;
        }
    }
}
