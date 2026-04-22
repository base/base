//! Contains the [`Validator`] trait which registers a validator for any bytes that
//! can be transformed into a [`crate::ValidatedUpdateData`] type.

use alloy_sol_types::{SolType, sol};

/// The Validator trait.
pub trait Validator<'a> {
    /// The input type accepted by this validator.
    type DataInput;

    /// Transforms the given bytes into an [`UpdateDataValidator`] type which can
    /// then be used to validate the update data for a specific update type.
    fn validate(&self, data: Self::DataInput) -> Result<ValidatedUpdateData<'a>, ValidationError>;
}

/// Validated `SystemConfig` update data.
///
/// After validation, this struct provides access to the validated pointer, length,
/// and the payload data starting at byte offset 64.
#[derive(Debug)]
pub struct ValidatedUpdateData<'a> {
    /// The full data bytes.
    data: &'a alloy_primitives::Bytes,
}

impl<'a> ValidatedUpdateData<'a> {
    /// Returns the payload slice (data starting from byte 64).
    #[inline]
    pub fn payload(&self) -> &[u8] {
        &self.data[64..]
    }
}

/// Common validation errors for `SystemConfig` updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// Invalid data length. Contains (expected, actual).
    InvalidDataLen(usize, usize),
    /// Failed to decode the pointer.
    PointerDecodingError,
    /// Invalid pointer value. Contains the actual value.
    InvalidDataPointer(u64),
    /// Failed to decode the length.
    LengthDecodingError,
    /// Invalid data length value. Contains the actual value.
    InvalidDataLength(u64),
}

/// The validator type performs the validation.
#[derive(Debug, Clone)]
pub struct UpdateDataValidator;

impl UpdateDataValidator {
    /// The expected data length for a standard `SystemConfigLog` update.
    pub const STANDARD_UPDATE_DATA_LEN: usize = 96;

    /// The expected pointer value for a standard `SystemConfigLog` update.
    pub const EXPECTED_POINTER: u64 = 32;

    /// The expected data length value for a standard `SystemConfigLog` update.
    pub const EXPECTED_DATA_LENGTH: u64 = 32;

    /// Validates the common structure of a `SystemConfig` update log data.
    ///
    /// This function performs the following validations:
    /// 1. Checks that the data length is exactly 96 bytes
    /// 2. Decodes and validates the pointer (must be 32)
    /// 3. Decodes and validates the data length field (must be 32)
    ///
    /// # Returns
    ///
    /// Returns a `ValidatedUpdateData` containing the validated fields and original data,
    /// or a `ValidationError` if any validation fails.
    pub fn validate(
        data: &alloy_primitives::Bytes,
    ) -> Result<ValidatedUpdateData<'_>, ValidationError> {
        // Validate total data length
        if data.len() != Self::STANDARD_UPDATE_DATA_LEN {
            return Err(ValidationError::InvalidDataLen(
                Self::STANDARD_UPDATE_DATA_LEN,
                data.len(),
            ));
        }

        // Decode and validate pointer
        let pointer = <sol!(uint64)>::abi_decode_validate(&data[0..32])
            .map_err(|_| ValidationError::PointerDecodingError)?;
        if pointer != Self::EXPECTED_POINTER {
            return Err(ValidationError::InvalidDataPointer(pointer));
        }

        // Decode and validate length
        let length = <sol!(uint64)>::abi_decode_validate(&data[32..64])
            .map_err(|_| ValidationError::LengthDecodingError)?;
        if length != Self::EXPECTED_DATA_LENGTH {
            return Err(ValidationError::InvalidDataLength(length));
        }

        Ok(ValidatedUpdateData { data })
    }
}

impl<'a, T> Validator<'a> for T
where
    T: AsRef<alloy_primitives::Bytes>,
{
    type DataInput = &'a alloy_primitives::Bytes;

    fn validate(&self, data: Self::DataInput) -> Result<ValidatedUpdateData<'a>, ValidationError> {
        UpdateDataValidator::validate(data)
    }
}
