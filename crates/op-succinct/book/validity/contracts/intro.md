# Contract Management

This section will show you how to configure and deploy the on-chain contracts required for OP Succinct in validity mode.

| Contract                       | Description                                                                                                                          |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------ |
| `OPSuccinctL2OutputOracle.sol` | Modified `L2OutputOracle.sol` with validity proof verification. Compatible with `OptimismPortal.sol`.                                |
| `OPSuccinctDisputeGame.sol`    | Validity proof verification contract implementing `IDisputeGame.sol` interface for `OptimismPortal2.sol`. Wraps the Oracle contract. |
