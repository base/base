/// Wrapper for [`ruint_macro::uint!`]. See its documentation for details.
#[macro_export]
#[cfg(not(doc))] // Show the actual macro in docs.
#[doc(hidden)]
macro_rules! uint {
    ($($t:tt)*) => {
        $crate::__private::ruint_macro::uint_with_path! { [$crate] $($t)* }
    }
}

macro_rules! impl_bin_op {
    ($trait:ident, $fn:ident, $trait_assign:ident, $fn_assign:ident, $fdel:ident) => {
        impl<const BITS: usize, const LIMBS: usize> $trait_assign<Uint<BITS, LIMBS>>
            for Uint<BITS, LIMBS>
        {
            #[inline(always)]
            #[track_caller]
            fn $fn_assign(&mut self, rhs: Uint<BITS, LIMBS>) {
                *self = self.$fdel(rhs);
            }
        }
        impl<const BITS: usize, const LIMBS: usize> $trait_assign<&Uint<BITS, LIMBS>>
            for Uint<BITS, LIMBS>
        {
            #[inline(always)]
            #[track_caller]
            fn $fn_assign(&mut self, rhs: &Uint<BITS, LIMBS>) {
                *self = self.$fdel(*rhs);
            }
        }
        impl<const BITS: usize, const LIMBS: usize> $trait<Uint<BITS, LIMBS>>
            for Uint<BITS, LIMBS>
        {
            type Output = Uint<BITS, LIMBS>;

            #[inline(always)]
            #[track_caller]
            fn $fn(self, rhs: Uint<BITS, LIMBS>) -> Self::Output {
                self.$fdel(rhs)
            }
        }
        impl<const BITS: usize, const LIMBS: usize> $trait<&Uint<BITS, LIMBS>>
            for Uint<BITS, LIMBS>
        {
            type Output = Uint<BITS, LIMBS>;

            #[inline(always)]
            #[track_caller]
            fn $fn(self, rhs: &Uint<BITS, LIMBS>) -> Self::Output {
                self.$fdel(*rhs)
            }
        }
        impl<const BITS: usize, const LIMBS: usize> $trait<Uint<BITS, LIMBS>>
            for &Uint<BITS, LIMBS>
        {
            type Output = Uint<BITS, LIMBS>;

            #[inline(always)]
            #[track_caller]
            fn $fn(self, rhs: Uint<BITS, LIMBS>) -> Self::Output {
                self.$fdel(rhs)
            }
        }
        impl<const BITS: usize, const LIMBS: usize> $trait<&Uint<BITS, LIMBS>>
            for &Uint<BITS, LIMBS>
        {
            type Output = Uint<BITS, LIMBS>;

            #[inline(always)]
            #[track_caller]
            fn $fn(self, rhs: &Uint<BITS, LIMBS>) -> Self::Output {
                self.$fdel(*rhs)
            }
        }
    };
}

macro_rules! assume {
    ($e:expr $(,)?) => {
        if !$e {
            debug_unreachable!(stringify!($e));
        }
    };

    ($e:expr, $($t:tt)+) => {
        if !$e {
            debug_unreachable!($($t)+);
        }
    };
}

macro_rules! debug_unreachable {
    ($($t:tt)*) => {
        if cfg!(debug_assertions) {
            panic!($($t)*);
        } else {
            unsafe { core::hint::unreachable_unchecked() };
        }
    };
}

macro_rules! const_range_for {
    ($p:pat in $start:tt.. $end:expr => $x:block) => {
        const_range_for!(@range $p, $start, $end, $x)
    };
    ($p:pat in ($start:expr).. $end:expr => $x:block) => {
        const_range_for!(@range $p, $start, $end, $x)
    };
    (@range $p:pat, $start:expr, $end:expr, $x:block) => {{
        let mut iter = $start;
        while iter < $end {
            // This mirrors `core::ops::IndexRange::next_unchecked` until const
            // fns can use normal `for` loops again.
            let $p = iter;
            // SAFETY: The range was checked to be non-empty, so adding one
            // cannot overflow.
            iter = unsafe { iter.unchecked_add(1) };
            $x
        }
    }};
    ($p:pat in ref $slice:expr => $x:block) => {{
        let slice = $slice;
        let range = slice.as_ptr_range();
        let mut ptr = range.start;
        let end = range.end;
        // This mirrors the non-ZST `core::slice::Iter::next` pointer walk until
        // const fns can use normal `for` loops again.
        while unsafe { end.offset_from(ptr) } != 0 {
            let old = ptr;
            // SAFETY: `ptr` is not `end`, so advancing by one stays within the
            // slice range or reaches the one-past-end pointer.
            ptr = unsafe { ptr.add(1) };
            // SAFETY: `old` came from `slice.as_ptr_range()` and was checked to
            // be before `end`, so it points to a live element.
            let $p = unsafe { &*old };
            $x
        }
    }};
    ($p:pat in rev ref $slice:expr => $x:block) => {{
        let slice = $slice;
        let range = slice.as_ptr_range();
        let start = range.start;
        let mut ptr = range.end;
        // This mirrors the non-ZST `core::slice::Iter::next_back` pointer walk
        // until const fns can use normal `for` loops again.
        while unsafe { ptr.offset_from(start) } != 0 {
            // SAFETY: `ptr` is after `start`, so moving back by one stays within
            // the slice range and points to the next element from the back.
            ptr = unsafe { ptr.sub(1) };
            // SAFETY: `ptr` came from `slice.as_ptr_range()` and was moved back
            // into the slice range before dereferencing.
            let $p = unsafe { &*ptr };
            $x
        }
    }};
    ($p:pat in mut $slice:expr => $x:block) => {{
        let slice = &mut $slice;
        let range = slice.as_mut_ptr_range();
        let mut ptr = range.start;
        let end = range.end;
        // This mirrors the non-ZST `core::slice::IterMut::next` pointer walk
        // until const fns can use normal `for` loops again.
        while unsafe { end.offset_from(ptr) } != 0 {
            let old = ptr;
            // SAFETY: `ptr` is not `end`, so advancing by one stays within the
            // slice range or reaches the one-past-end pointer.
            ptr = unsafe { ptr.add(1) };
            // SAFETY: `old` came from `slice.as_mut_ptr_range()` and was checked
            // to be before `end`. The pointer is advanced before yielding, which
            // matches `IterMut` and avoids yielding the same element twice.
            let $p = unsafe { &mut *old };
            $x
        }
    }};
}

/// `let $id = &mut [0u64; nlimbs(2 * BITS)][..]`
macro_rules! let_double_bits {
    ($id:ident) => {
        // This array casting is a workaround for `generic_const_exprs` not being
        // stable.
        let mut double = [[0u64; 2]; LIMBS];
        let double_len = crate::nlimbs(2 * BITS);
        debug_assert!(2 * LIMBS >= double_len);
        // SAFETY: `[[u64; 2]; LIMBS] == [u64; 2 * LIMBS] >= [u64; nlimbs(2 * BITS)]`.
        let $id = unsafe {
            core::slice::from_raw_parts_mut(double.as_mut_ptr().cast::<u64>(), double_len)
        };
    };
}

/// Specialize an operation for u64, u128, u256 ([u128; 2])...
macro_rules! as_primitives {
    ($($uints:expr),* $(,)?; $($rest:tt)*) => {
        as_primitives!(@inner ($($uints),*); $($rest)*);
    };

    (@inner $uints:tt; { $($arm:ident $t:tt => $e:expr),* $(,)? }) => {
        $(
            as_primitives!(@arm $uints; $arm $t => $e);
        )*
    };

    (@arm ($($uint:expr),*); u64($($n:pat),*) => $e:expr) => {
        if LIMBS == 1 {
            $(
                let $n = $uint.limbs[0];
            )*
            $e
        }
    };
    (@arm ($($uint:expr),*); u128($($n:pat),*) => $e:expr) => {
        if LIMBS == 2 {
            $(
                let $n = $uint.as_double_words()[0].get();
            )*
            $e
        }
    };
    (@arm ($($uint:expr),*); u256($(($lo:pat, $hi:pat)),*) => $e:expr) => {
        if LIMBS == 4 {
            $(
                let &[lo, hi] = $uint.as_double_words() else { unreachable!() };
                let $lo = lo.get();
                let $hi = hi.get();
            )*
            $e
        }
    };
}

#[cfg(test)]
mod tests {
    // https://github.com/alloy-rs/ruint/issues/359
    ruint_macro::uint_with_path! {
        [crate]
        const _A: [crate::aliases::U256; 2] = [
            0x00006f85d6f68a85ec10345351a23a3aaf07f38af8c952a7bceca70bd2af7ad5_U256,
            0x00004b4110c9ae997782e1509b1d0fdb20a7c02bbd8bea7305462b9f8125b1e8_U256,
        ];
    }

    crate::uint! {
        const _B: [crate::aliases::U256; 2] = [
            0x00006f85d6f68a85ec10345351a23a3aaf07f38af8c952a7bceca70bd2af7ad5_U256,
            0x00004b4110c9ae997782e1509b1d0fdb20a7c02bbd8bea7305462b9f8125b1e8_U256,
        ];
    }

    #[test]
    fn test_uint_macro_with_paths() {
        extern crate self as aaa;
        use crate as ruint;
        use crate as __ruint;
        let value = crate::aliases::U256::from(0x10);
        assert_eq!(value, uint!(0x10U256));
        assert_eq!(value, ruint_macro::uint_with_path!([crate] 0x10U256));
        assert_eq!(value, ruint_macro::uint_with_path!([aaa] 0x10U256));
        assert_eq!(value, ruint_macro::uint_with_path!([aaa] 0x10U256));
        assert_eq!(value, ruint_macro::uint_with_path!([ruint] 0x10U256));
        assert_eq!(value, ruint_macro::uint_with_path!([__ruint] 0x10U256));
    }
}
