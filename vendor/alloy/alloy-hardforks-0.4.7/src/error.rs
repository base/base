#[derive(Debug)]
/// Error type for hardfork related errors.
pub struct ParseHardforkError(alloc::string::String);

impl ParseHardforkError {
    /// Creates a new hardfork parse error with the given message
    pub fn new<S: Into<alloc::string::String>>(msg: S) -> Self {
        Self(msg.into())
    }

    /// Returns the error message
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for ParseHardforkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl core::error::Error for ParseHardforkError {}
