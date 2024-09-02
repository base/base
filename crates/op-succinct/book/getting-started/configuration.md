# Configuration

The last step is to update your OP Stack configuration to use the new `ZKL2OutputOracle` contract managed by the `op-succinct-proposer` service.

## Self-Managed OP Stack Chains

If you are using a self-managed OP Stack chain, you will need to use your `ADMIN` key to update the existing `L2OutputOracle` implementation. Recall that the `L2OutputOracle` is a proxy contract that is upgradeable using the `ADMIN` key.

To update the `L2OutputOracle` implementation, you can use an existing script we have in the `op-succinct` repo:

```bash
forge script script/ZKUpgrader.s.sol:ZKUpgrader \
    --rpc-url $L1_RPC \
    --private-key $PRIVATE_KEY \
    --verify \
    --verifier etherscan \
    --etherscan-api-key $ETHERSCAN_API_KEY \
    --broadcast \
    --ffi
```

## RaaS Providers