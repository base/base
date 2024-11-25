# Getting Started

In this section, we'll guide you through upgrading an OP Stack chain to a [fully type-1 ZK rollup](https://vitalik.eth.limo/general/2022/08/04/zkevm.html) using SP1 and OP Succinct. 

The steps are the following:
1) **Run the cost estimator** to confirm that all of your endpoints are set up correctly. The cost estimator will simulate the proving of the chain and return the estimated "costs" of proving.
2) **Run OP Succinct in mock mode** to confirm that your on-chain configuration is correct.
3) **Run OP Succinct in full mode** to begin generating proofs of `op-succinct`.
4) **Upgrade your OP Stack configuration** to upgrade the `L2OutputOracle` contract to the new `OPSuccinctL2OutputOracle` contract using your `ADMIN` key.

![Getting Started](../assets/upgrading-op-stack.jpg)
