use std::{ffi::OsStr, fmt, marker::PhantomData, str::FromStr};

use clap::{Arg, Command, Error, builder::TypedValueParser, error::ErrorKind};

/// Signing material, redacted in diagnostic output.
#[derive(Clone)]
pub struct Secret<T>(pub T);

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Parses signing material without including input values in clap errors.
#[derive(Debug, Clone)]
pub struct SecretValueParser<T>(pub PhantomData<T>);

impl<T> Default for SecretValueParser<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T: FromStr + Clone + Send + Sync + 'static> TypedValueParser for SecretValueParser<T> {
    type Value = Secret<T>;

    fn parse_ref(
        &self,
        _cmd: &Command,
        _arg: Option<&Arg>,
        value: &OsStr,
    ) -> Result<Self::Value, Error> {
        // clap's FromStr adapter echoes the raw value on error, even if T's error is redacted.
        value.to_str().and_then(|s| s.parse().ok()).map(Secret).ok_or_else(|| {
            Error::raw(ErrorKind::ValueValidation, "invalid signing secret; check its format")
        })
    }
}
