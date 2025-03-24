# Repository Structure

```
op-succinct
├── contracts      # Smart contracts for OP Succinct
│   ├── validity   # Validity contracts
│   ├── fp         # Fault proof contracts
├── validity       # Validity proposer and challenger implementation
├── fault-proof    # Fault proof system implementation
├── programs       # Range and aggregation program implementations
├── utils          # Shared utilities
│   ├── client     # Client-side utilities
│   ├── host       # Host-side utilities
│   └── build      # Build utilities
├── scripts        # Development and deployment scripts
└── elf            # Reproducible binaries
```

## Contracts

The `contracts` directory contains the core contracts for OP Succinct.

### Validity

- `OPSuccinctL2OutputOracle.sol`: Modified version of `L2OutputOracle.sol` that implements validity proof verification. Compatible with `OptimismPortal.sol`.
- `OPSuccinctDisputeGame.sol`: Validity proof verification contract that is compatiable with the `IDisputeGame.sol` interface on `OptimismPortal2.sol`. Wraps the `OPSuccinctL2OutputOracle.sol` contract.

### Fault Proof

The `fault-proof` directory contains the contracts for OP Succinct Lite.

- `OPSuccinctFaultDisputeGame.sol`: Fault proof verification contract that is compatiable with the `IDisputeGame.sol` interface on `OptimismPortal2.sol`. Implements the OP Succinct Lite fault proof challenge mechanism.
- `AccessManager.sol`: Contract that manages access control for the fault proof system.
