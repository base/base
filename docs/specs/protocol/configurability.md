# OP Stack Configurability

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Consensus Parameters](#consensus-parameters)
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

## Consensus Parameters

| Config Property                       | Description                                                                                                                  | Administrator                       |
|---------------------------------------|------------------------------------------------------------------------------------------------------------------------------|-------------------------------------|
| [Batch Inbox address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L176)                   | L1 address where calldata/blobs are posted (see [Batcher Transaction](../glossary.md#batcher-transaction)).                  | Static |
| [Batcher Hash](./system_config.md#batcherhash-bytes32) | A versioned hash of the current authorized batcher sender(s). | [System Config Owner](#admin-roles) |
| [Chain ID](https://github.com/ethereum-optimism/superchain-registry/blob/main/superchain/configs/chainids.json)                              | Unique ID of Chain used for TX signature validation.                                                                         | Static |
| [Challenge Period](https://github.com/ethereum-optimism/superchain-registry/pull/44)                      | Length of time for which an output root can be removed, and for which it is not considered finalized.                        | [L1 Proxy Admin](#admin-roles)                      |
| [Fee Scalar](./system_config.md#scalars)                            | Markup on transactions compared to the raw L1 data cost.                                                                     | [System Config Owner](#admin-roles)                 |
| [Gas Limit](./system_config.md#gaslimit-uint64) | Gas limit of the L2 blocks is configured through the system config. | [System Config Owner](#admin-roles) |
| Genesis state                         | Initial state at chain genesis, including code and storage of predeploys (all L2 smart contracts). See [Predeploy](../glossary.md#l2-genesis-block). | Static |
| [L2 block time](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/L2OutputOracle.sol#L105)                         | Frequency with which blocks are produced as a result of derivation.                                                          | [L1 Proxy Admin](#admin-roles)                      |
| [Resource config](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L338-L340)                       | Config for the EIP-1559 based curve used for the deposit gas market.                                                         | [System Config Owner](#admin-roles)                 |
| Sequencing window                     | Maximum allowed batch submission gap, after which L1 fallback is triggered in derivation.                                    | Static |
| [Start block](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L184)                           | Block at which the system config was initialized the first time.                                                             | [L1 Proxy Admin](#admin-roles)                      |
| Superchain target                     | Choice of cross-L2 configuration. May be omitted in isolated OP Stack deployments. Includes SuperchainConfig and ProtocolVersions contract addresses. | Static |

## Policy Parameters

| Config Property                       | Description                                                                                                                  | Administrator                       |
|---------------------------------------|------------------------------------------------------------------------------------------------------------------------------|-------------------------------------|
| Data Availability Type        | Batcher can currently be configured to use blobs or calldata (See [Data Availability Provider](../glossary.md#data-availability-provider)).             | [Batch submitter address](#service-roles)                 |
| Batch submission frequency            | Frequency with which batches are submitted to L1 (see [Batcher Transaction](../glossary.md#batcher-transaction)).            | [Batch submitter address](#service-roles)                 |                   |
| [Output frequency](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/L2OutputOracle.sol#L104)                      | Frequency with which output roots are submitted to L1.                                                                       | [L1 Proxy Admin](#admin-roles)                      |

## Admin Roles

| Config Property                       | Description                                                                                                                  | Administrator                       | Administers                         |
|---------------------------------------|------------------------------------------------------------------------------------------------------------------------------|-------------------------------------|-------------------------------------|
| [L1 Proxy Admin](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/universal/ProxyAdmin.sol#L30)                        | Account authorized to upgrade L1 contracts.                                                                                  | [L1 Proxy Admin Owner](#admin-roles)                | [Batch Inbox Address](#consensus-parameters), [Start block](#consensus-parameters), [Proposer address](#service-roles), [Challenger address](#service-roles), [Guardian address](#service-roles), [Challenge Period](#consensus-parameters), [Output frequency](#policy-parameters), [L2 block time](#consensus-parameters), [L1 smart contracts](#consensus-parameters)
| L1 ProxyAdmin owner                   | Account authorized to update the L1 Proxy Admin.                                                                             |                                     | [L1 Proxy Admin](#admin-roles)
| [L2 Proxy Admin](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/universal/ProxyAdmin.sol#L30)                        | Account authorized to upgrade L2 contracts.                                                                                  | [L2 Proxy Admin Owner](#admin-roles)                | [Predeploys](./predeploys.md#overview)
| L2 ProxyAdmin owner                   | Account authorized to update the L2 Proxy Admin.                                                                             |                                     | [L2 Proxy Admin](#admin-roles)
| [System Config Owner](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L14C26-L14C44)                   | Account authorized to change values in the SystemConfig contract. All configuration is stored on L1 and picked up by L2 as part of the [derviation](./derivation.md) of the L2 chain. |                                     | [Batch submitter address](#service-roles), [Sequencer P2P / Unsafe head signer](#service-roles), [Fee Margin](#consensus-parameters), [Gas limit](#consensus-parameters), [Resource config](#consensus-parameters), [System Config Owner](#admin-roles)

## Service Roles

| Config Property                       | Description                                                                                                                  | Administrator                       |
|---------------------------------------|------------------------------------------------------------------------------------------------------------------------------|-------------------------------------|
| [Batch submitter address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L265)               | Account which authenticates new batches submitted to L1 Ethereum.                                                            | [System Config Owner](#admin-roles)                 |
| [Challenger address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/L2OutputOracle.sol#L109)                    | Account which can delete output roots before challenge period has elapsed.                                                   | [L1 Proxy Admin](#admin-roles)                      |
| [Guardian address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SuperchainConfig.sol#L50)                      | Account authorized to pause L1 withdrawals from contracts.                                                                   | [L1 Proxy Admin](#admin-roles)                      |
| [Proposer address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/L2OutputOracle.sol#L108)                      | Account which can propose output roots to L1.                                                                                | [L1 Proxy Admin](#admin-roles)                      |
| [Sequencer P2P / Unsafe head signer](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L250)    | Account which authenticates the unsafe/pre-submitted blocks for a chain at the P2P layer.                                    | [System Config Owner](#admin-roles)                 |
