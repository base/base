# Configuration

## Overview

The last step is to update your OP Stack configuration to use the new `OPSuccinctL2OutputOracle` contract managed by the `op-succinct-proposer` service.

> ⚠️ **Caution**: When upgrading to the `OPSuccinctL2OutputOracle` contract, maintain the existing `finalizationPeriod` for a duration equal to at least one `finalizationPeriod`. Failure to do so will result in immediate finalization of all pending output roots upon upgrade, which is unsafe. Only after this waiting period has elapsed should you set the `finalizationPeriod` to 0.

## Upgrading `L2OutputOracle`

If your OP Stack chain's admin is a multi-sig or contract, you will need to use your `ADMIN` key to update the existing `L2OutputOracle` implementation. Recall that the `L2OutputOracle` is a proxy contract that is upgradeable using the `ADMIN` key.

### EOA `ADMIN` key

To update the `L2OutputOracle` implementation with an EOA `ADMIN` key, run the following command in `/contracts`.

```bash
forge script script/OPSuccinctUpgrader.s.sol:OPSuccinctUpgrader \
    --rpc-url $L1_RPC \
    --private-key $PRIVATE_KEY \
    --verify \
    --verifier etherscan \
    --etherscan-api-key $ETHERSCAN_API_KEY \
    --broadcast \
    --ffi
```

### `ADMIN` key is not an EOA

If the owner of the `L2OutputOracle` is not an EOA (e.g. multisig, contract), set `EXECUTE_UPGRADE_CALL` to `false`. This will output the raw calldata for the upgrade call, which can be executed by the owner.

```bash
EXECUTE_UPGRADE_CALL=false forge script script/OPSuccinctUpgrader.s.sol:OPSuccinctUpgrader \
    --rpc-url $L1_RPC \
    --private-key $PRIVATE_KEY \
    --verify \
    --verifier etherscan \
    --etherscan-api-key $ETHERSCAN_API_KEY \
    --broadcast \
    --ffi

...
== Logs ==
  Upgrade calldata:
  0x3659cfe60000000000000000000000003af9a0224e5370f31c07e6739c76b32d75b2d4af
  Update contract parameter calldata:
  0x7ad016520000000000000000000000000000000000000000000000000000000000003b03002d397eaa6f2bd3a873f2b996a6d486eb20774092e68a75471e287084180c133237870c3fe7a735661b52f641bd41c85a886c916a962526533c8c9d17dc08310000000000000000000000003b6041173b80e77f038f3f2c0f9744f04837185e7ca9e1e9829e0e28c934debd1adab0592b4a906d48b01d750ec46c02d09ad833
```
