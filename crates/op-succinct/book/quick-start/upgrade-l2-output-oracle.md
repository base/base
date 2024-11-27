# Migrate `L2OutputOracle` to `OPSuccinctL2OutputOracle`

The last step is to update your OP Stack configuration to use the new `OPSuccinctL2OutputOracle` contract managed by the `op-succinct` service.

> ⚠️ **Caution**: When upgrading to the `OPSuccinctL2OutputOracle` contract, maintain the existing `finalizationPeriod` for a duration equal to at least one `finalizationPeriod`. Failure to do so will result in immediate finalization of all pending output roots upon upgrade, which is unsafe. Only after this waiting period has elapsed should you set the `finalizationPeriod` to 0.

To upgrade an existing OP Stack deployment to use the `OPSuccinctL2OutputOracle` contract, follow the instructions in the [Upgrading `OPSuccinctL2OutputOracle`](../advanced/l2-output-oracle.md#upgrading-opsuccinctl2outputoracle) section.
