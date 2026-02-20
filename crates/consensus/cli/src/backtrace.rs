//! Helper to set the backtrace env var.

/// Sets the `RUST_BACKTRACE` environment variable to 1 if it is not already set.
pub fn enable() {
    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        // SAFETY: We accept the risk of a race with other threads setting env vars; this is
        // a best-effort initialization that only runs once at startup.
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }
}
