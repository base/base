# Deploying a Smart Contract to Base

This guide walks through deploying a simple Solidity smart contract to Base mainnet or Base Sepolia using Foundry or Hardhat. It also covers verification on Basescan and common pitfalls.

## Prerequisites

- A funded wallet on Base mainnet or Base Sepolia
- `foundry` or `hardhat` installed
- RPC endpoint for Base (e.g., Alchemy, Infura, or public RPC)
- Basescan API key for contract verification

## Chain Parameters

| Network    | Chain ID | RPC URL                       | Explorer                    |
|------------|----------|-------------------------------|-----------------------------|
| Base       | `8453`   | `https://mainnet.base.org`    | `https://basescan.org`      |
| Base Sepolia | `84532` | `https://sepolia.base.org`   | `https://sepolia.basescan.org` |

## Option 1: Deploy with Foundry

### 1. Install Foundry

```bash
curl -L https://foundry.paradigm.xyz | bash
foundryup
```

### 2. Create a Project

```bash
forge init my-base-contract
cd my-base-contract
```

### 3. Write a Simple Contract

Create `src/HelloBase.sol`:

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract HelloBase {
    string public greeting;

    constructor(string memory _greeting) {
        greeting = _greeting;
    }

    function setGreeting(string memory _greeting) external {
        greeting = _greeting;
    }
}
```

### 4. Configure Deployment

Create `.env`:

```bash
BASE_RPC_URL=https://sepolia.base.org
PRIVATE_KEY=0xYOUR_PRIVATE_KEY
ETHERSCAN_API_KEY=YOUR_BASESCAN_API_KEY
```

Add `.env` to `.gitignore`.

### 5. Write a Deploy Script

Create `script/Deploy.s.sol`:

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script} from "forge-std/Script.sol";
import {HelloBase} from "../src/HelloBase.sol";

contract DeployScript is Script {
    function run() external {
        vm.startBroadcast(msg.sender);
        new HelloBase("Hello, Base!");
        vm.stopBroadcast();
    }
}
```

### 6. Deploy

```bash
forge script script/Deploy.s.sol:DeployScript \
  --rpc-url $BASE_RPC_URL \
  --private-key $PRIVATE_KEY \
  --broadcast \
  --verify \
  --etherscan-api-key $ETHERSCAN_API_KEY
```

## Option 2: Deploy with Hardhat

### 1. Initialize Project

```bash
mkdir my-base-contract && cd my-base-contract
npm init -y
npm install --save-dev hardhat @nomicfoundation/hardhat-toolbox
npx hardhat init
```

### 2. Configure `hardhat.config.ts`

```ts
import { HardhatUserConfig } from "hardhat/config";
import "@nomicfoundation/hardhat-toolbox";

const config: HardhatUserConfig = {
  solidity: "0.8.20",
  networks: {
    base: {
      url: process.env.BASE_RPC_URL || "https://mainnet.base.org",
      accounts: [process.env.PRIVATE_KEY || ""],
      chainId: 8453,
    },
    baseSepolia: {
      url: process.env.BASE_RPC_URL || "https://sepolia.base.org",
      accounts: [process.env.PRIVATE_KEY || ""],
      chainId: 84532,
    },
  },
  etherscan: {
    apiKey: process.env.ETHERSCAN_API_KEY,
    customChains: [
      {
        network: "base",
        chainId: 8453,
        urls: {
          apiURL: "https://api.basescan.org/api",
          browserURL: "https://basescan.org",
        },
      },
      {
        network: "baseSepolia",
        chainId: 84532,
        urls: {
          apiURL: "https://api-sepolia.basescan.org/api",
          browserURL: "https://sepolia.basescan.org",
        },
      },
    ],
  },
};

export default config;
```

### 3. Compile and Deploy

```bash
npx hardhat compile
npx hardhat run scripts/deploy.ts --network baseSepolia
```

Example `scripts/deploy.ts`:

```ts
import { ethers } from "hardhat";

async function main() {
  const HelloBase = await ethers.getContractFactory("HelloBase");
  const contract = await HelloBase.deploy("Hello, Base!");
  await contract.waitForDeployment();
  console.log("Deployed to:", await contract.getAddress());
}

main().catch(console.error);
```

### 4. Verify

```bash
npx hardhat verify --network baseSepolia DEPLOYED_CONTRACT_ADDRESS "Hello, Base!"
```

## Common Pitfalls

- **Insufficient funds**: Ensure the deployer wallet has enough ETH for gas on the target network.
- **Wrong chain ID**: Double-check the chain ID before broadcasting transactions.
- **RPC rate limits**: Public RPCs may throttle. Use a dedicated provider for production deployments.
- **Verification timeout**: Basescan verification can take a few minutes. Retry if it fails.
- **Contract size limit**: Base enforces EIP-170 contract size limits. Keep contracts under ~24 KB or use proxy patterns.

## Next Steps

- Read more about Base in the existing guides: `RELEASE.md`, `UPGRADES.md`, `P2P.md`
- Visit [docs.base.org](https://docs.base.org) for deeper protocol details
