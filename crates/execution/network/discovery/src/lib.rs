#![doc = include_str!("../README.md")]

mod nat;
pub use nat::{NatResolver, ParseNatResolverError, ResolveNatInterval};

mod net_if;
pub use net_if::{DEFAULT_NET_IF_NAME, NetInterfaceError, NetworkInterface};
