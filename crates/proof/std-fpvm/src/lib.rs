#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://raw.githubusercontent.com/ethereum-optimism/optimism/develop/rust/kona/assets/favicon.ico",
    issue_tracker_base_url = "https://github.com/ethereum-optimism/optimism/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(target_arch = "mips64", feature(asm_experimental_arch))]
#![cfg_attr(any(target_arch = "mips64", target_arch = "riscv64"), no_std)]

extern crate alloc;

pub mod errors;

pub mod io;

#[cfg(feature = "tracing")]
mod fpvm_tracing;
#[cfg(feature = "tracing")]
pub use fpvm_tracing::FpvmTracingSubscriber;

pub mod malloc;

mod traits;
pub use traits::BasicKernelInterface;

mod types;
pub use types::FileDescriptor;

mod channel;
pub use channel::FileChannel;

mod linux;

#[cfg(target_arch = "mips64")]
mod mips64;

#[cfg(target_arch = "riscv64")]
mod riscv64;
