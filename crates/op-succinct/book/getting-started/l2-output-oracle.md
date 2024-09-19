# Deploy L2 Output Oracle

The first step in deploying OP Succinct is to deploy a Solidity smart contract that will verify ZKPs of OP derivation (OP's name for their state transition function) and contain the latest state root of your rollup.


## Deployment

### 1) Clone the `op-succinct` repo:

```bash
git clone https://github.com/succinctlabs/op-succinct.git
cd op-succinct
```

### 2) Set environment variables:

```bash
cp .env.example .env
```

Set the following environment variables:

```bash
L1_RPC=...
L2_RPC=...
L2_NODE_RPC=...
PRIVATE_KEY=...
ETHERSCAN_API_KEY=...
```

### 3) Navigate to the contracts directory:

```bash
cd contracts
```

### 4) Set Deployment Parameters

Inside the `contracts` folder there is a file called `zkl2ooconfig.json` that contains the parameters for the deployment. You will need to fill it with your chain's specific details.

The following parameters are required: `proposer`, `challenger`, `finalizationPeriod`, `owner`, `verifierGateway`. The rest of the fields (`startingBlockNumber`, `l2BlockTime`, `chainId` and `vkey`) are automatically fetched by the `fetch-rollup-config` script which is invoked by the `ZKDeployer` forge script. To use a manually set `startingBlockNumber`, set `USE_CACHED_STARTING_BLOCK` to `true`.


| Parameter | Description |
|-----------|-------------|
| `startingBlockNumber` | The L2 block number at which to start generating validity proofs. This should be set to the current L2 block number. You can fetch this with `cast bn --rpc-url <L2_RPC>`. |
| `submissionInterval` | The number of L2 blocks between each L1 output submission. |
| `l2BlockTime` | The time in seconds between each L2 block. |
| `proposer` | The Ethereum address of the proposer account. If `address(0)`, anyone can submit proofs. |
| `challenger` | The Ethereum address of the challenger account. If `address(0)`, no one can dispute proofs. |
| `finalizationPeriod` | The time period (in seconds) after which a proposed output becomes finalized. Specifically, the time period after which you can withdraw your funds against the proposed output. |
| `chainId` | The chain ID of the L2 network. |
| `owner` | The Ethereum address of the `ZKL2OutputOracle` owner, who can update the verification key and verifier address. |
| `vkey` | The verification key for the aggregate program. Run `cargo run --bin vkey --release` to generate this. |
| `rollupConfigHash` | The hash of the rollup config. This is used for non-superchain OP stack configurations. |
| `verifierGateway` | The address of the verifier gateway contract. The canonical Succinct verifiers can be found [here](https://docs.succinct.xyz/onchain-verification/contract-addresses.html). |
| `l2OutputOracleProxy` | The address of your OP Stack chain's L2 Output Oracle proxy contract which will be upgraded. Only used in `ZKUpgrader`. |

### 5) Deploy the `ZKL2OutputOracle` contract:

This foundry script will deploy the `ZKL2OutputOracle` contract to the specified L1 RPC and use the provided private key to sign the transaction:

```bash
forge script script/ZKDeployer.s.sol:ZKDeployer \
    --rpc-url $L1_RPC \
    --private-key $PRIVATE_KEY \
    --ffi \
    --verify \
    --verifier etherscan \
    --etherscan-api-key $ETHERSCAN_API_KEY \
    --broadcast
```

If successful, you should see the following output:

```
Script ran successfully.

== Return ==
0: address 0x9b520F7d8031d45Eb8A1D9fE911038576931ab95

## Setting up 1 EVM.

==========================

Chain 11155111

Estimated gas price: 11.826818849 gwei

Estimated total gas used for script: 3012823

Estimated amount required: 0.035632111845100727 ETH

==========================

##### sepolia
✅  [Success]Hash: 0xc57d97ac588563406183969e8ea15bc06496915547114b1df4e024c142df07b4
Contract Address: 0x2e4a7Dc6F19BdE1edF1040f855909afF7CcBeDeC
Block: 6633852
Paid: 0.00858210364707003 ETH (1503205 gas * 5.709203766 gwei)


##### sepolia
✅  [Success]Hash: 0x1343094b0be4e89594aedb57fb795d920e7cc1a76288485e8cf248fa206321ed
Block: 6633852
Paid: 0.001907479233443196 ETH (334106 gas * 5.709203766 gwei)


##### sepolia
✅  [Success]Hash: 0x708ce24c69c2637cadd6cffc654cbe2114e9ea4ec1e69838cd45c1fa27981713
Contract Address: 0x9b520F7d8031d45Eb8A1D9fE911038576931ab95
Block: 6633852
Paid: 0.00250654027540581 ETH (439035 gas * 5.709203766 gwei)

✅ Sequence #1 on sepolia | Total Paid: 0.012996123155919036 ETH (2276346 gas * avg 5.709203766 gwei)
                                                                                                          

==========================

ONCHAIN EXECUTION COMPLETE & SUCCESSFUL.
##
Start verification for (2) contracts
Start verifying contract `0x9b520F7d8031d45Eb8A1D9fE911038576931ab95` deployed on sepolia

Submitting verification for [lib/optimism/packages/contracts-bedrock/src/universal/Proxy.sol:Proxy] 0x9b520F7d8031d45Eb8A1D9fE911038576931ab95.

...
```

Keep note of the address of the `Proxy` contract that was deployed, which in this case is `0x9b520F7d8031d45Eb8A1D9fE911038576931ab95`. 

It is also returned by the script as `0: address 0x9b520F7d8031d45Eb8A1D9fE911038576931ab95`. 