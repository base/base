# Toggle Optimistic Mode

Optimistic mode is a feature that allows the L2OutputOracle to accept outputs without verification (mirroring the original `L2OutputOracle` contract). This is useful for testing and development purposes, and as a fallback for when `OPSuccinctL2OutputOracle` is unable to verify outputs.

When optimistic mode is enabled, the `OPSuccinctL2OutputOracle`'s `proposeL2Output` function will match the interface of the original L2OutputOracle contract, with the modification that the proposer address must be in the `approvedProposers` mapping, or permissionless proposing must be enabled.

## Enable Optimistic Mode

To enable optimistic mode, call the `enableOptimisticMode` function on the `OPSuccinctL2OutputOracle` contract.

```solidity
function enableOptimisticMode(uint256 _finalizationPeriodSeconds) external onlyOwner whenNotOptimistic {
    finalizationPeriodSeconds = _finalizationPeriodSeconds;
    optimisticMode = true;
    emit OptimisticModeToggled(true, _finalizationPeriodSeconds);
}
```

Ensure that the `finalizationPeriodSeconds` is set to a value that is appropriate for your use case. The standard setting is 1 week (604800 seconds).

The `finalizationPeriodSeconds` should never be 0.

## Disable Optimistic Mode

By default, optimistic mode is disabled. To switch back to validity mode, call the `disableOptimisticMode` function on the `OPSuccinctL2OutputOracle` contract.

```solidity
function disableOptimisticMode(uint256 _finalizationPeriodSeconds) external onlyOwner whenOptimistic {
    finalizationPeriodSeconds = _finalizationPeriodSeconds;
    optimisticMode = false;
    emit OptimisticModeToggled(false, _finalizationPeriodSeconds);
}
```

Set the `finalizationPeriodSeconds` to a value that is appropriate for your use case. An example configuration is 1 hour (3600 seconds).

The `finalizationPeriodSeconds` should never be 0.
