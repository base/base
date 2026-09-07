#![doc = include_str!("../README.md")]
#![allow(missing_docs)]
#![cfg_attr(not(test), warn(unused_extern_crates))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod traits;
#[doc(no_inline)]
pub use eyre;
#[doc(no_inline)]
pub use ruint::{self, aliases::U256, uint};
pub use traits::*;

mod pointer;
pub use pointer::{Pointer, PointerBase};

/// Compilation result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Compilation error.
pub type Error = eyre::Error;

/// Returns a [`Display`](std::fmt::Display) formatter for a byte count in human-readable form
/// (e.g. `"1.5 KiB"`, `"3.2 MiB"`).
pub fn format_bytes(bytes: usize) -> impl std::fmt::Display {
    std::fmt::from_fn(move |f| {
        if bytes < 1024 {
            write!(f, "{bytes} B")
        } else if bytes < 1024 * 1024 {
            write!(f, "{:.1} KiB ({bytes} B)", bytes as f64 / 1024.0)
        } else {
            write!(f, "{:.1} MiB ({bytes} B)", bytes as f64 / (1024.0 * 1024.0))
        }
    })
}
