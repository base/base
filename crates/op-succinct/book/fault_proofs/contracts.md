# Contract Management

This section will show you how to configure and deploy the on-chain contracts required for OP Succinct in fault-proof mode.

| Contract                         | Description                                                                                                                                                         |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `OPSuccinctFaultDisputeGame.sol` | Fault proof verification contract implementing `IDisputeGame.sol` interface for `OptimismPortal2.sol`. Implements OP Succinct Lite fault proof challenge mechanism. |
| `AccessManager.sol`              | Manages access control for the fault proof system.                                                                                                                  |
