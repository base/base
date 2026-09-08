# Base EVM context

Shared execution context, environments, journaling, and context interfaces for the Base EVM.

Concrete types and their interfaces live together. The crate supports `no_std`; node
configuration and storage-provider implementations remain outside this layer.
