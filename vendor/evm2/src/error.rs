//! Error code handles for host errors.

use alloc::{string::String, sync::Arc};
use core::{error::Error, fmt, num::NonZeroUsize};

/// Type-erased host error.
#[derive(Clone, Debug)]
pub struct AnyError(Arc<dyn Error + Send + Sync>);

impl fmt::Display for AnyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Error for AnyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

impl PartialEq for AnyError {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for AnyError {}

impl AnyError {
    /// Creates a new [`AnyError`] from any error type.
    pub fn new(err: impl Error + Send + Sync + 'static) -> Self {
        Self(Arc::new(err))
    }

    /// Returns the original error when it has type `E`.
    pub fn downcast_ref<E: Error + 'static>(&self) -> Option<&E> {
        self.0.downcast_ref()
    }
}

struct StringError(String);

impl Error for StringError {}

impl fmt::Display for StringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

// Purposefully skip printing "StringError(..)"
impl fmt::Debug for StringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl From<String> for AnyError {
    fn from(value: String) -> Self {
        Self::new(StringError(value))
    }
}

impl From<&str> for AnyError {
    fn from(value: &str) -> Self {
        Self::new(StringError(value.into()))
    }
}

/// Lightweight handle for a host error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ErrorCode(NonZeroUsize);

impl ErrorCode {
    /// Number of reserved internal error codes.
    pub const RESERVED_COUNT: usize = 255;

    pub(crate) const STORED_ERROR: Self = Self::new_reserved(1);

    /// Reserved code signalling that a cold load was skipped because the caller could not afford
    /// the cold access.
    pub const COLD_LOAD_SKIPPED: Self = Self::new_reserved(2);

    /// Reserved code signalling that precompile execution stopped on a fatal error.
    pub const FATAL_PRECOMPILE: Self = Self::new_reserved(3);

    /// Reserved code signalling that a read is not covered by the attached EIP-7928 Block Access
    /// List and database fallback is disabled, so the BAL is invalid for this access.
    pub const BAL_NOT_COVERED: Self = Self::new_reserved(4);

    #[inline]
    const fn new_reserved(code: usize) -> Self {
        assert!(code > 0 && code <= Self::RESERVED_COUNT);
        Self::new_raw(code).unwrap()
    }

    #[inline]
    const fn new_raw(code: usize) -> Option<Self> {
        let Some(code) = NonZeroUsize::new(code) else {
            return None;
        };
        Some(Self(code))
    }

    /// Creates a custom error code outside the reserved internal range.
    #[inline]
    pub const fn new_custom(code: usize) -> Option<Self> {
        let Some(code) = code.checked_add(Self::RESERVED_COUNT + 1) else {
            return None;
        };
        Self::new_raw(code)
    }

    /// Returns the raw error code.
    #[inline]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

pub(crate) fn error_unavailable(code: ErrorCode) -> AnyError {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct ErrorUnavailable(ErrorCode);

    impl fmt::Display for ErrorUnavailable {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "error code {:?} is unavailable", self.0)
        }
    }

    impl Error for ErrorUnavailable {}

    AnyError::new(ErrorUnavailable(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_codes_stay_in_reserved_range() {
        assert_eq!(ErrorCode::STORED_ERROR.get(), 1);
        assert_eq!(ErrorCode::COLD_LOAD_SKIPPED.get(), 2);
        assert_eq!(ErrorCode::FATAL_PRECOMPILE.get(), 3);
        assert!(ErrorCode::FATAL_PRECOMPILE.get() <= ErrorCode::RESERVED_COUNT);
    }

    #[test]
    fn custom_codes_start_after_reserved_range() {
        assert_eq!(ErrorCode::new_custom(0).unwrap().get(), ErrorCode::RESERVED_COUNT + 1);
        assert_eq!(ErrorCode::new_custom(1).unwrap().get(), ErrorCode::RESERVED_COUNT + 2);
    }

    #[test]
    fn custom_code_overflow_returns_none() {
        let last_custom = usize::MAX - ErrorCode::RESERVED_COUNT - 1;
        assert_eq!(ErrorCode::new_custom(last_custom).unwrap().get(), usize::MAX);
        assert!(ErrorCode::new_custom(last_custom + 1).is_none());
    }
}
