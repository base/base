# Metering RPC

`base_meterBlockByHash` and `base_meterBlockByNumber` re-execute a block against its
parent state and report signer recovery time, execution time, and per-transaction
measurements. Enable these endpoints with `--enable-metering`.

`base_setMeteringInformation` accepts a transaction hash and one `TransactionResult`
record, including `gasUsed`, `executionTimeUs`, and optional `opcodeGas` observations.
The builder evaluates that transaction's observations against its configured resource
schedule. A record with a different transaction hash cannot supply a resource sample.
`base_setMeteringEnabled` controls the observation store and
`base_clearMeteringInformation` clears it.

Bundle simulation and the `base_meterBundle`, `eth_callBundle`, and `mev_simBundle`
endpoints have been removed. EVM state accumulation and L1 blob processing are unchanged.

The ignored legacy `--metering.gas-limit`, `--metering.execution-time-us`,
`--metering.state-root-time-us`, and `--metering.da-bytes` flags are no longer
accepted. Configure resource limits with `--payload.resource-metering-schedule`.
The ignored builder flags `--builder.block-state-root-gas-limit`,
`--builder.state-root-gas-coefficient`, and `--builder.state-root-gas-anchor-us`
have also been removed.
