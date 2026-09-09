# RPC transports and registry

`RpcRegistryInner` constructs the supported API modules from the Base node components.
`TransportRpcModuleConfig` selects their HTTP/WS exposure, and `RpcServerConfig` starts the
transports with the configured addresses, limits, authentication, CORS, and middleware.
Base node startup registers its built-in transaction, bundle, metering, and proof APIs directly.
