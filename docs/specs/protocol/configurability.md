# OP Stack Configurability

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Consensus Parameters](#consensus-parameters)
  - [Resource Config](#resource-config)
- [Policy Parameters](#policy-parameters)
- [Admin Roles](#admin-roles)
- [Service Roles](#service-roles)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

When deploying the OP Stack software to a production environment,
certain parameters about the protocol can be configured. These
configurations can include a variety of parameters which affect the
resulting properties of the blockspace in question.

There are four categories of OP Stack configuration options:

- **Consensus Parameters**: Parameters and properties of the chain that may
  be set at genesis and fixed for the lifetime of the chain, or may be
  changeable through a privileged account or protocol upgrade.
- **Policy Parameters**: Parameters of the chain that might be changed without
  breaking consensus. Consensus is enforced by the protocol, while policy parameters
  may be changeable within constraints imposed by consensus.
- **Admin Roles**: These roles can upgrade contracts, change role owners,
  or update protocol parameters. These are typically cold/multisig wallets not
  used directly in day to day operations.
- **Service Roles**: These roles are used to manage the day-to-day
  operations of the chain, and therefore are often hot wallets.

Each category also defines the standard configuration values for it's given parameters.
Standard configuration is the set of requirements for an OP Stack chain to be considered a
Standard Chain within the superchain.
These requirements are currently a draft, pending governance approval.

## Consensus Parameters

| Config Property                       | Description                                                                                                                  | Administrator                       | Standard Config Requirement | Notes |
|---------------------------------------|------------------------------------------------------------------------------------------------------------------------------|-------------------------------------|-------------------------------------|-------------------------------------|
| [Batch Inbox address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L176)                   | L1 address where calldata/blobs are posted (see [Batcher Transaction](../glossary.md#batcher-transaction)).                  | Static | Current convention is <code>versionByte &vert;&vert; keccak256(bytes32(chainId))[:19]</code>, where <code>&vert;&vert;</code> denotes concatenation, `versionByte` is `0x00`, and `chainId` is a `uint256`. | It is recommended, but not required, to follow this convention. |
| [Batcher Hash](./system_config.md#batcherhash-bytes32) | A versioned hash of the current authorized batcher sender(s). | [System Config Owner](#admin-roles) | `bytes32(uint256(uint160(batchSubmitterAddress)))` | [Batch Submitter](../protocol/batcher.md) address padded with zeros to fit 32 bytes. |
| [Chain ID](https://github.com/ethereum-optimism/superchain-registry/blob/main/superchain/configs/chainids.json)                              | Unique ID of Chain used for TX signature validation.                                                                         | Static | Foundation-approved, globally unique value [^chain-id]. | Foundation will ensure chains are responsible with their chain IDs until there's a governance process in place. |
| [Challenge Period](../protocol/withdrawals.md#withdrawal-flow)                      | Length of time for which an output root can be removed, and for which it is not considered finalized.                        | [L1 Proxy Admin](#admin-roles)                      | 7 days | High security. Excessively safe upper bound that leaves enough time to consider social layer solutions to a hack if necessary. Allows enough time for other network participants to challenge the integrity of the corresponding output root. |
| [Fee Scalar](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L288-L294)                            | Markup on transactions compared to the raw L1 data cost.                                                                     | [System Config Owner](#admin-roles)                 | Set such that Fee Margin is between 0 and 50%. |  |
| [Gas Limit](./system_config.md#gaslimit-uint64) | Gas limit of the L2 blocks is configured through the system config. | [System Config Owner](#admin-roles) | No higher than 200_000_000 gas | Chain operators are driven to maintain a stable and reliable chain. When considering to change this value, careful deliberation is necessary. |
| Genesis state                         | Initial state at chain genesis, including code and storage of predeploys (all L2 smart contracts). See [Predeploy](../glossary.md#l2-genesis-block). | Static | Only standard predeploys and preinstalls, no additional state. | Homogeneity & standardization, ensures initial state is secure. |
| [L2 block time](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/L2OutputOracle.sol#L105)                         | Frequency with which blocks are produced as a result of derivation.                                                          | [L1 Proxy Admin](#admin-roles)                      | 2 seconds | High security & [interoperability](../interop/overview.md) compatibility requirement, until de-risked/solved at app layer. |
| [Resource config](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L338-L340)                       | Config for the EIP-1559 based curve used for the deposit gas market.                                                         | [L1 Proxy Admin](#admin-roles)                 | See [resource config table](#resource-config). | Constraints are imposed in [code](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L345-L365) when setting the resource config. |
| [Sequencing window Size](../glossary.md#sequencing-window)                  | Maximum allowed batch submission gap, after which L1 fallback is triggered in derivation.                                    | Static | 3_600 base layer blocks (12 hours for an L2 on Ethereum, assuming 12 second L1 blocktime). e.g. 12 second blocks, $3600 * 12\ seconds \div 60\frac{seconds}{minute} \div 60\frac{minute}{hour} = 12\ hours$. | This is an important value for constraining the sequencer's ability to re-order transactions; higher values would pose a risk to user protections. |
| [Start block](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L184)                           | Block at which the system config was initialized the first time.                                                             | [L1 Proxy Admin](#admin-roles)                      | The block where the SystemConfig was initialized. | Simple clear restriction. |
| [Superchain target](../protocol/superchain-upgrades.md#superchain-target)  | Choice of cross-L2 configuration. May be omitted in isolated OP Stack deployments. Includes SuperchainConfig and ProtocolVersions contract addresses. | Static | Mainnet or Sepolia | A superchain target defines a set of layer 2 chains which share `SuperchainConfig` and `ProtocolVersions` contracts deployed on layer 1. |

[^chain-id]: The chain ID must be globally unique among all EVM chains.

### Resource Config

| Config Property                       | Standard Config Requirement |
|---------------------------------------|------------------------------------------------------------------------------------------------------------------------------|
| maxResourceLimit | $2*10^7$ | |
| elasticityMultiplier | $10$ | |
| baseFeeMaxChangeDenominator | $8$ | |
| minimumBaseFee | $1*10^9$ | |
| systemTxMaxGas | $1*10^6$ | |
| maximumBaseFee | $2^{128}$-1 | |

## Policy Parameters

| Config Property                       | Description                                                                                                                  | Administrator                       | Standard Config Requirement | Notes |
|---------------------------------------|------------------------------------------------------------------------------------------------------------------------------|-------------------------------------|-------------------------------------|-------------------------------------|
| Data Availability Type        | Batcher can currently be configured to use blobs or calldata (See [Data Availability Provider](../glossary.md#data-availability-provider)).             | [Batch submitter address](#service-roles)                 | Ethereum (Blobs, Calldata) | Alt-DA is not yet supported for the standard configuration, but the sequencer can switch at-will between blob and calldata with no restriction, since both are L1 security. |
| Batch submission frequency            | Frequency with which batches are submitted to L1 (see [Batcher Transaction](../glossary.md#batcher-transaction)).            | [Batch submitter address](#service-roles)                 | 1_800 base layer blocks (6 hours for an L2 on Ethereum, assuming 12 second L1 blocktime) or lower | Batcher needs to fully submit each batch within the sequencing window, so this leaves buffer to account for L1 network congestion and the amount of data the batcher would need to post. |
| [Output frequency](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/L2OutputOracle.sol#L104)                      | Frequency with which output roots are submitted to L1.                                                                       | [L1 Proxy Admin](#admin-roles)                      | 43_200 L2 Blocks (24 hours for an L2 on Ethereum, assuming 2 second L2 blocktime) or lower e.g. 2 second blocks, $43200 * 2\ seconds \div 60\frac{seconds}{minute} \div 60\frac{minute}{hour} = 24\ hours$. | Deprecated once fault proofs are implemented. This value cannot be 0. |

## Admin Roles

| Config Property                       | Description                                                                                                                  | Administrator                       | Administers                         | Standard Config Requirement | Notes |
|---------------------------------------|------------------------------------------------------------------------------------------------------------------------------|-------------------------------------|-------------------------------------|-------------------------------------|-------------------------------------|
| L1 Proxy Admin | Account authorized to upgrade L1 contracts. | [L1 Proxy Admin Owner](#admin-roles) | [Batch Inbox Address](#consensus-parameters), [Start block](#consensus-parameters), [Proposer address](#service-roles), [Challenger address](#service-roles), [Guardian address](#service-roles), [Challenge Period](#consensus-parameters), [Output frequency](#policy-parameters), [L2 block time](#consensus-parameters), [L1 smart contracts](#consensus-parameters) | [ProxyAdmin.sol](https://github.com/ethereum-optimism/optimism/blob/op-contracts/v1.3.0/packages/contracts-bedrock/src/universal/ProxyAdmin.sol) from the latest `op-contracts/vX.Y.X` release of source code in [Optimism repository](https://github.com/ethereum-optimism/optimism). | Governance-controlled, high security. |
| L1 ProxyAdmin owner                   | Account authorized to update the L1 Proxy Admin.  | | [L1 Proxy Admin](#admin-roles) | [0x5a0Aae59D09fccBdDb6C6CcEB07B7279367C3d2A](https://etherscan.io/address/0x5a0Aae59D09fccBdDb6C6CcEB07B7279367C3d2A) [^of-sc-gnosis-safe-l1] | Governance-controlled, high security. |
| L2 Proxy Admin | Account authorized to upgrade L2 contracts. | [L2 Proxy Admin Owner](#admin-roles) | [Predeploys](./predeploys.md#overview) | [ProxyAdmin.sol](https://github.com/ethereum-optimism/optimism/blob/op-contracts/v1.3.0/packages/contracts-bedrock/src/universal/ProxyAdmin.sol) from the latest `op-contracts/vX.Y.X` release of source code in [Optimism repository](https://github.com/ethereum-optimism/optimism). Predeploy address:  [0x4200000000000000000000000000000000000018](https://docs.optimism.io/chain/addresses#op-mainnet-l2). | Governance-controlled, high security. |
| L2 ProxyAdmin owner                   | Account authorized to upgrade protocol contracts via calls to the `ProxyAdmin`. This is the aliased L1 ProxyAdmin owner address.                                                                            |                                     | [L2 Proxy Admin](#admin-roles) | Gnosis Safe between Optimism Foundation (OF) and the Security Council (SC). Aliased Address: [0x6B1BAE59D09fCcbdDB6C6cceb07B7279367C4E3b](https://optimistic.etherscan.io/address/0x6B1BAE59D09fCcbdDB6C6cceb07B7279367C4E3b) [^aliased-of-sc-gnosis-safe-l1] | Governance-controlled, high security. |
| [System Config Owner](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L14C26-L14C44)                   | Account authorized to change values in the SystemConfig contract. All configuration is stored on L1 and picked up by L2 as part of the [derivation](./derivation.md) of the L2 chain. |                                     | [Batch submitter address](#service-roles), [Sequencer P2P / Unsafe head signer](#service-roles), [Fee Margin](#consensus-parameters), [Gas limit](#consensus-parameters), [System Config Owner](#admin-roles) | Chain Governor or Servicer | As defined in the [Law of Chains](https://github.com/ethereum-optimism/OPerating-manual/blob/main/Law%20of%20Chains.md) |

[^of-sc-gnosis-safe-l1]: 2 of 2 GnosisSafe between Optimism Foundation (OF) and the Security Council (SC) on L1. Mainnet and Sepolia addresses can be found at [privileged roles](https://docs.optimism.io/chain/security/privileged-roles#l1-proxy-admin).
[^aliased-of-sc-gnosis-safe-l1]: Aliased address of the 2 of 2 Gnosis Safe between Optimism Foundation (OF) and the Security Council (SC) on L1. The reason for aliasing can be found in the [glossary](../glossary.md#address-aliasing). This address was calculated using the following arithmetic: `0x5a0Aae59D09fccBdDb6C6CcEB07B7279367C3d2A` + `0x1111000000000000000000000000000000001111` = `0x6B1BAE59D09fCcbdDB6C6cceb07B7279367C4E3b`.

## Service Roles

| Config Property                       | Description                                                                                                                  | Administrator                       | Standard Config Requirement | Notes |
|---------------------------------------|------------------------------------------------------------------------------------------------------------------------------|-------------------------------------|-------------------------------------|-------------------------------------|
| [Batch submitter address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L265)               | Account which authenticates new batches submitted to L1 Ethereum.                                                            | [System Config Owner](#admin-roles)                 | No requirement | |
| [Challenger address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/L2OutputOracle.sol#L109)                    | Account which can delete output roots before challenge period has elapsed.                                                   | [L1 Proxy Admin](#admin-roles)                      | [0x9BA6e03D8B90dE867373Db8cF1A58d2F7F006b3A](https://etherscan.io/address/0x9BA6e03D8B90dE867373Db8cF1A58d2F7F006b3A) [^of-gnosis-safe-l1] | Optimism Foundation (OF) multisig leveraging [battle-tested software](https://github.com/safe-global/safe-smart-account). |
| [Guardian address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SuperchainConfig.sol#L50)                      | Account authorized to pause L1 withdrawals from contracts.                                                                   | [L1 Proxy Admin](#admin-roles)                      | [0x09f7150D8c019BeF34450d6920f6B3608ceFdAf2](https://etherscan.io/address/0x09f7150D8c019BeF34450d6920f6B3608ceFdAf2) | A 1/1 Safe owned by the Security Council Safe, with the [Deputy Guardian Module](../experimental/security-council-safe.md#deputy-guardian-module) enabled to allow the Optimism Foundation to act as Guardian. |
| [Proposer address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/L2OutputOracle.sol#L108)                      | Account which can propose output roots to L1.                                                                                | [L1 Proxy Admin](#admin-roles)                      | No requirement | |
| [Sequencer P2P / Unsafe head signer](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L250)    | Account which authenticates the unsafe/pre-submitted blocks for a chain at the P2P layer.                                    | [System Config Owner](#admin-roles)                 | No requirement | |

[^of-gnosis-safe-l1]: 5 of 7 GnosisSafe controlled by Optimism Foundation (OF). Mainnet and Sepolia addresses can be found at [privileged roles](https://docs.optimism.io/chain/security/privileged-roles).
