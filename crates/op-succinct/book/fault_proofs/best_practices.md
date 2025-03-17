# Best Practices

This document covers best practices for OP Succinct Lite deployment setup.

## SafeDB Configuration

### Enabling SafeDB in op-node

SafeDB is a critical component for efficient L1 head determination. When SafeDB is not enabled, the system falls back to timestamp-based L1 head estimation, which can lead to several issues:

1. Less reliable derivation
2. Potential cycle count blowup for derivation

To enable SafeDB in your op-node, see [Consensus layer configuration options (op-node)](https://docs.optimism.io/operators/node-operators/configuration/consensus-config#safedbpath).

This ensures that L1 head can be efficiently determined without relying on the more expensive fallback mechanism.
