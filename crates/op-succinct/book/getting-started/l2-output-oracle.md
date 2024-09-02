# L2 Output Oracle

The first step in deploying OP Succinct is to deploy a Solidity smart contract that will verify ZKPs of OP derivation (OP's name for their state transition function) and contain the latest state root of your rollup.


## Deployment

### 1) Clone the `op-succinct` repo:

```bash
git clone https://github.com/succinctlabs/op-succinct.git
cd op-succinct
```

### 2) Navigate to the contracts directory:

```bash
cd contracts
```

### 3) Set Deployment Parameters

Inside the `contracts` folder there is a file called `zkconfig.json` that contains the parameters for the deployment. You will need to fill it with your chain's specific details.


| Parameter | Description |
|-----------|-------------|
| `startingBlockNumber` | The L2 block number at which the rollup starts. Default should be 0. |
| `l2RollupNode` | The URL of the L2 rollup node. (After the tutorial, this is `http://localhost:8545`) |
| `submissionInterval` | The number of L2 blocks between each L1 output submission. |
| `l2BlockTime` | The time in seconds between each L2 block. |
| `proposer` | The Ethereum address of the proposer account. If `address(0)`, anyone can submit proofs. |
| `challenger` | The Ethereum address of the challenger account. If `address(0)`, no one can dispute proofs. |
| `finalizationPeriod` | The time period (in seconds) after which a proposed output becomes finalized. Specifically, the time period after which you can withdraw your funds against the proposed output. |
| `chainId` | The chain ID of the L2 network. |
| `owner` | The Ethereum address of the `ZKL2OutputOracle` owner, who can update the verification key and verifier address. |
| `vkey` | The verification key for the aggregate program. Run `cargo run --bin vkey --release` to generate this. |
| `verifierGateway` | The address of the verifier gateway contract. |
| `l2OutputOracleProxy` | The address of your OP Stack chain's L2 Output Oracle proxy contract which will be upgraded. |

### 4) Deploy the `ZKL2OutputOracle` contract:

This foundry script will deploy the `ZKL2OutputOracle` contract to the specified L1 RPC and use the provided private key to sign the transaction.

Make sure to set your env with the following variables:

```
L1_RPC=...
L2_NODE_RPC=...
PRIVATE_KEY=...
ETHERSCAN_API_KEY=...
```

and then run the following command to deploy the contract:

```bash
forge script script/ZKDeployer.s.sol:ZKDeployer \
    --rpc-url $L1_RPC \
    --private-key $PRIVATE_KEY \
    --verify \
    --verifier etherscan \
    --etherscan-api-key $ETHERSCAN_API_KEY \
    --broadcast \
    --ffi
```

If successful, you should see the following output:

```
Submitting verification for [src/ZKL2OutputOracle.sol:ZKL2OutputOracle] 0xfe6BcbCD9c067d937431b54AfF107D4F8f2aC653.
Submitted contract for verification:
        Response: `OK`
        GUID: `qyc71u8whqpuf3cylumh3bdcf39a4nzv6ffpubfqef2jrzserr`
        URL: https://sepolia.etherscan.io/address/0xfe6bcbcd9c067d937431b54aff107d4f8f2ac653
Contract verification status:
Response: `NOTOK`
Details: `Pending in queue`
Contract verification status:
Response: `NOTOK`
Details: `Already Verified`
Contract source code already verified
All (2) contracts were verified!
```

Keep note of the address of the `ZKL2OutputOracle` contract that was deployed. You will need it in the next few sections.