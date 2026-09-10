#![doc = include_str!("../README.md")]

mod trusted_proxy;
pub use trusted_proxy::{ForwardedClientIpError, TrustedProxyConfig};
