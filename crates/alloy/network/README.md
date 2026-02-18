## `base-alloy-network`

Base chain network types and RPC behavior abstraction.

This crate contains a simple abstraction of the RPC behavior of a Base chain. It is intended to be used
by the Alloy client to provide a consistent interface to the rest of the library, regardless of
changes the underlying blockchain makes to the RPC interface.
