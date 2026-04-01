//! Atomic operation shims for the riscv32im zkVM target.
//!
//! The riscv32im ISA lacks the "A" (atomics) extension, so LLVM emits calls to
//! `__atomic_*` builtins for any `core::sync::atomic` usage. The risc0 zkVM is
//! single-threaded, so these can be safely implemented as plain memory
//! operations.

use core::ptr;

#[unsafe(no_mangle)]
unsafe extern "C" fn __atomic_load_1(src: *const u8, _ordering: i32) -> u8 {
    unsafe { ptr::read_volatile(src) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn __atomic_load_4(src: *const u32, _ordering: i32) -> u32 {
    unsafe { ptr::read_volatile(src) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn __atomic_store_1(dst: *mut u8, val: u8, _ordering: i32) {
    unsafe { ptr::write_volatile(dst, val) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn __atomic_store_4(dst: *mut u32, val: u32, _ordering: i32) {
    unsafe { ptr::write_volatile(dst, val) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn __atomic_fetch_add_4(dst: *mut u32, val: u32, _ordering: i32) -> u32 {
    unsafe {
        let old = ptr::read_volatile(dst);
        ptr::write_volatile(dst, old.wrapping_add(val));
        old
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn __atomic_fetch_sub_4(dst: *mut u32, val: u32, _ordering: i32) -> u32 {
    unsafe {
        let old = ptr::read_volatile(dst);
        ptr::write_volatile(dst, old.wrapping_sub(val));
        old
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn __atomic_compare_exchange_1(
    dst: *mut u8,
    expected: *mut u8,
    desired: u8,
    _weak: bool,
    _success_ordering: i32,
    _failure_ordering: i32,
) -> bool {
    unsafe {
        let old = ptr::read_volatile(dst);
        if old == ptr::read(expected) {
            ptr::write_volatile(dst, desired);
            true
        } else {
            ptr::write(expected, old);
            false
        }
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn __atomic_compare_exchange_4(
    dst: *mut u32,
    expected: *mut u32,
    desired: u32,
    _weak: bool,
    _success_ordering: i32,
    _failure_ordering: i32,
) -> bool {
    unsafe {
        let old = ptr::read_volatile(dst);
        if old == ptr::read(expected) {
            ptr::write_volatile(dst, desired);
            true
        } else {
            ptr::write(expected, old);
            false
        }
    }
}
