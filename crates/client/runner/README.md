# base-reth-runner

This crate hosts the Base-specific node launcher that wires together the Optimism node
components, execution extensions, and RPC add-ons that run inside the Base node binary.
It exposes the types that the CLI uses to build a node and pass those pieces to Optimism's
`Cli` runner.
