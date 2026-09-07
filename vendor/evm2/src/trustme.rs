#![allow(clippy::missing_transmute_annotations)]
#![allow(clippy::missing_safety_doc)]

use crate::{EvmTypesHost, interpreter::Interpreter};

/// Changes the lifetime of the given reference.
pub(crate) unsafe fn decouple_lt<'a, T: ?Sized>(x: &T) -> &'a T {
    unsafe { core::mem::transmute(x) }
}

/// Changes the lifetime of the given mutable reference.
pub(crate) unsafe fn decouple_lt_mut<'a, T: ?Sized>(x: &mut T) -> &'a mut T {
    unsafe { core::mem::transmute(x) }
}

/// Changes the lifetime of the given mutable pointer.
pub(crate) const unsafe fn decouple_lt_mut_ptr<T, U>(x: *mut T) -> *mut U {
    x.cast::<U>()
}

/// Changes the lifetime of the given box.
pub(crate) unsafe fn decouple_lt_box<T, U>(x: alloc::boxed::Box<T>) -> alloc::boxed::Box<U> {
    unsafe { core::mem::transmute(x) }
}

/// Changes the lifetime of an interpreter reference stored in the pool.
pub(crate) unsafe fn decouple_interpreter_lt_mut<'pool, 'frame, 'host, T: EvmTypesHost>(
    x: &'pool mut Interpreter<'static, 'static, T>,
) -> &'pool mut Interpreter<'frame, 'host, T> {
    unsafe {
        core::mem::transmute::<
            &'pool mut Interpreter<'static, 'static, T>,
            &'pool mut Interpreter<'frame, 'host, T>,
        >(x)
    }
}
