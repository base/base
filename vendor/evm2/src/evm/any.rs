use core::any::TypeId;

/// Provides lifetime-erased type identity for erased EVM objects.
///
/// `TypeId` does not distinguish lifetime parameters, so checked downcasting is only exposed for
/// erased objects whose concrete value is known to be `'static`.
pub trait NonStaticAny {
    /// Returns the lifetime-erased type ID of `self`.
    #[inline]
    fn type_id(&self) -> TypeId {
        typeid::of::<Self>()
    }
}

impl<T: ?Sized> NonStaticAny for T {}

impl dyn NonStaticAny + '_ {
    /// Downcasts this object to a concrete type without checking the type.
    ///
    /// # Safety
    ///
    /// The erased object must be a valid `T`. For concrete types with lifetime parameters, those
    /// lifetimes must match the erased value's actual lifetimes.
    #[inline]
    pub unsafe fn downcast_ref_unchecked<T: NonStaticAny>(&self) -> &T {
        unsafe { &*(self as *const Self).cast::<T>() }
    }

    /// Mutably downcasts this object to a concrete type without checking the type.
    ///
    /// # Safety
    ///
    /// The erased object must be a valid `T`. For concrete types with lifetime parameters, those
    /// lifetimes must match the erased value's actual lifetimes.
    #[inline]
    pub unsafe fn downcast_mut_unchecked<T: NonStaticAny>(&mut self) -> &mut T {
        unsafe { &mut *(self as *mut Self).cast::<T>() }
    }
}

impl dyn NonStaticAny + 'static {
    /// Returns whether this object has type `T`.
    #[inline]
    pub fn is<T: NonStaticAny + 'static>(&self) -> bool {
        self.type_id() == typeid::of::<T>()
    }

    /// Downcasts this object to a concrete type.
    #[inline]
    pub fn downcast_ref<T: NonStaticAny + 'static>(&self) -> Option<&T> {
        self.is::<T>().then(|| {
            // SAFETY: The `TypeId` check above verified the concrete type, and this API is only
            // available on `'static` erased objects and targets, so lifetimes cannot be erased into
            // a longer borrow.
            unsafe { self.downcast_ref_unchecked() }
        })
    }

    /// Mutably downcasts this object to a concrete type.
    #[inline]
    pub fn downcast_mut<T: NonStaticAny + 'static>(&mut self) -> Option<&mut T> {
        self.is::<T>().then(|| {
            // SAFETY: The `TypeId` check above verified the concrete type, and this API is only
            // available on `'static` erased objects and targets, so lifetimes cannot be erased into
            // a longer borrow.
            unsafe { self.downcast_mut_unchecked() }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Borrowing<'a> {
        value: &'a mut u8,
    }

    #[test]
    fn unchecked_downcast_ref_works_with_borrowed_type() {
        let mut value = 1;
        let borrowing = Borrowing { value: &mut value };
        let erased = &borrowing as &dyn NonStaticAny;

        let downcasted = unsafe { erased.downcast_ref_unchecked::<Borrowing<'_>>() };

        assert_eq!(*downcasted.value, 1);
    }

    #[test]
    fn unchecked_downcast_mut_works_with_borrowed_type() {
        let mut value = 1;
        {
            let mut borrowing = Borrowing { value: &mut value };
            let erased = &mut borrowing as &mut dyn NonStaticAny;
            let downcasted = unsafe { erased.downcast_mut_unchecked::<Borrowing<'_>>() };
            *downcasted.value = 2;
        }

        assert_eq!(value, 2);
    }

    #[test]
    fn checked_downcast_ref_checks_static_type() {
        let value = 1u8;
        let erased: &(dyn NonStaticAny + 'static) = &value;

        assert_eq!(*erased.downcast_ref::<u8>().unwrap(), value);
        assert!(erased.downcast_ref::<u16>().is_none());
    }

    #[test]
    fn checked_downcast_mut_checks_static_type() {
        let mut value = 1u8;
        {
            let erased: &mut (dyn NonStaticAny + 'static) = &mut value;
            *erased.downcast_mut::<u8>().unwrap() = 2;
            assert!(erased.downcast_mut::<u16>().is_none());
        }

        assert_eq!(value, 2);
    }
}
