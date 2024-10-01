# OP Stack Configurability

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Consensus Parameters](#consensus-parameters)
  - [Batch Inbox address](#batch-inbox-address)
  - [Batcher Hash](#batcher-hash)
  - [Chain ID](#chain-id)
  - [Proof Maturity Delay](#proof-maturity-delay)
  - [Dispute Game Finality](#dispute-game-finality)
  - [Respected Game Type](#respected-game-type)
  - [Fault Game Max Depth](#fault-game-max-depth)
  - [Fault Game Split Depth](#fault-game-split-depth)
  - [Max Game Clock Duration](#max-game-clock-duration)
  - [Game Clock Extension](#game-clock-extension)
  - [Bond Withdrawal Delay](#bond-withdrawal-delay)
  - [Minimum Large Preimage Proposal Size](#minimum-large-preimage-proposal-size)
  - [Large Preimage Proposal Challenge Period](#large-preimage-proposal-challenge-period)
  - [Fault Game Absolute Prestate](#fault-game-absolute-prestate)
  - [Fault Game Genesis Block](#fault-game-genesis-block)
  - [Fault Game Genesis Output Root](#fault-game-genesis-output-root)
  - [Fee Scalar](#fee-scalar)
  - [Gas Limit](#gas-limit)
  - [Genesis state](#genesis-state)
  - [L2 block time](#l2-block-time)
  - [Resource config](#resource-config)
  - [Sequencing window Size](#sequencing-window-size)
  - [Start block](#start-block)
  - [Superchain target](#superchain-target)
  - [Governance Token](#governance-token)
  - [Resource Config](#resource-config)
- [Policy Parameters](#policy-parameters)
  - [Data Availability Type](#data-availability-type)
  - [Batch submission frequency](#batch-submission-frequency)
  - [Output frequency](#output-frequency)
- [Admin Roles](#admin-roles)
  - [L1 Proxy Admin](#l1-proxy-admin)
  - [L1 ProxyAdmin owner](#l1-proxyadmin-owner)
  - [L2 Proxy Admin](#l2-proxy-admin)
  - [L2 ProxyAdmin owner](#l2-proxyadmin-owner)
  - [System Config Owner](#system-config-owner)
- [Service Roles](#service-roles)
  - [Batch submitter address](#batch-submitter-address)
  - [Challenger address](#challenger-address)
  - [Guardian address](#guardian-address)
  - [Proposer address](#proposer-address)
  - [Sequencer P2P / Unsafe head signer](#sequencer-p2p--unsafe-head-signer)

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

The recommended way to deploy L1 contracts for an OP chain that meet the standard configuration will be with the
[OP Contracts Manager](../experimental/op-contracts-manager.md).

## Consensus Parameters

### [Batch Inbox address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L176)

**Description:** L1 address where calldata/blobs are posted (
see [Batcher Transaction](../glossary.md#batcher-transaction)).<br/>
**Administrator:** Static<br/>
**Requirement:** Current convention is
<code>versionByte &vert;&vert; keccak256(bytes32(chainId))\[:19\]</code>, where <code>&vert;&vert;</code> denotes
concatenation, `versionByte` is `0x00`, and `chainId` is a `uint256`.<br/>
**Notes:**  It is recommended, but not required, to follow this convention.

### [Batcher Hash](./system-config.md#batcherhash-bytes32)

**Description:** A versioned hash of the current authorized batcher sender(s).<br/>
**Administrator:** [System Config Owner](#admin-roles)<br/>
**Requirement:** `bytes32(uint256(uint160(batchSubmitterAddress)))`<br/>
**Notes**: [Batch Submitter](../protocol/batcher.md) address padded with zeros to fit 32 bytes.<br/>

### [Chain ID](https://github.com/ethereum-optimism/superchain-registry/blob/2a011e700e8be22bc18502f3d41c440e7a05015d/chainList.json)

**Description:** Unique ID of Chain used for TX signature validation.<br/>
**Administrator:** Static<br/>
**Requirement:** Foundation-approved, globally unique value [^chain-id-uniqueness].<br/>
**Notes:** Foundation will ensure chains are responsible with their chain IDs until there's a governance process in
place.<br/>

### [Proof Maturity Delay](../fault-proof/stage-one/bridge-integration.md#fpac-optimismportal-mods-specification)

**Description:** The length of time that must pass between proving and finalizing a withdrawal.<br/>
**Administrator:** [L1 Proxy Admin](#admin-roles)<br/>
**Requirement:** 7 days<br/>
**Notes:** High security. Excessively safe upper bound that leaves enough time to consider social layer solutions to a
hack if necessary. Allows enough time for other network participants to challenge the integrity of the corresponding
output root.<br/>

### [Dispute Game Finality](../fault-proof/stage-one/bridge-integration.md#fpac-optimismportal-mods-specification)

**Description:** The amount of time given to the `Guardian` role
to [blacklist a resolved dispute game](../fault-proof/stage-one/bridge-integration.md#blacklisting-disputegames) before
any withdrawals proven against it can be finalized, in the case of a system failure.<br/>
**Administrator:** [L1 Proxy Admin](#admin-roles)<br/>
**Requirement:** 3.5 days<br/>
**Notes:** High security. Allows enough time for the `Guardian` to blacklist games.<br/>

### [Respected Game Type](../fault-proof/stage-one/bridge-integration.md#new-state-variables)

**Description:** The respected game type of the `OptimismPortal`. Determines the type of dispute games that can be used
to finalize withdrawals.<br/>
**Administrator:** [Guardian](#service-roles)<br/>
**Requirement:** [`CANNON` (
`0`)](https://github.com/ethereum-optimism/optimism/blob/op-contracts/v1.5.0/packages/contracts-bedrock/src/dispute/lib/Types.sol#L28)<br/>
**Notes:** The game type may be changed to [`PERMISSIONED_CANNON` (
`1`)](https://github.com/ethereum-optimism/optimism/blob/op-contracts/v1.5.0/packages/contracts-bedrock/src/dispute/lib/Types.sol#L31)
as a fallback to permissioned proposals, in the event of a failure in the Fault Proof system.<br/>

### Fault Game Max Depth

**Description:** The maximum depth of fault
dispute [game trees](https://specs.optimism.io/fault-proof/stage-one/fault-dispute-game.html#game-tree).<br/>
**Administrator:** Static<br/>
**Requirement:** 73<br/>
**Notes:** Sufficiently large to ensure the fault proof VM execution trace fits within the number of leaf nodes.<br/>

### Fault Game Split Depth

**Description:** The depth in fault
dispute [game trees](https://specs.optimism.io/fault-proof/stage-one/fault-dispute-game.html#game-tree) after which
claims correspond to VM state commitments instead of output root commitments.<br/>
**Administrator:** Static<br/>
**Requirement:** 30<br/>
**Notes:** Sufficiently large to ensure enough nodes at the split depth to represent all L2 blocks since the anchor
state.<br/>

### [Max Game Clock Duration](https://specs.optimism.io/fault-proof/stage-one/fault-dispute-game.html#max_clock_duration)

**Description:** The maximum amount of time that may accumulate on a dispute game team's chess clock.<br/>
**Administrator:** Static<br/>
**Requirement:** 3.5 days<br/>
**Notes:** High security. Allows enough time for honest actors to counter invalid claims.<br/>

### [Game Clock Extension](https://specs.optimism.io/fault-proof/stage-one/fault-dispute-game.html#clock_extension)

**Description:** The flat credit that is given to a dispute game team's clock if their clock has less than
CLOCK_EXTENSION seconds remaining.<br/>
**Administrator:** Static<br/>
**Requirement:** 3 hours<br/>
**Notes:** Allows enough time for honest actors to counter freeloader claims.<br/>

### [Bond Withdrawal Delay](https://specs.optimism.io/fault-proof/stage-one/bond-incentives.html#delay-period)

**Description:** The length of time that must pass before dispute game bonds can be withdrawn.<br/>
**Administrator:** Static<br/>
**Requirement:** 7 days<br/>
**Notes:** High security. Allows enough time for the `Guardian` to recover funds from `DelayedWETH` if bonds were
allocated incorrectly.<br/>

### [Minimum Large Preimage Proposal Size](https://specs.optimism.io/fault-proof/stage-one/fault-dispute-game.html#preimageoracle-interaction)

**Description:** The minimum size of preimage allowed to be submitted via the PreimageOracle large preimage proposal
process.<br/>
**Administrator:** Static<br/>
**Requirement:** 126000 bytes<br/>
**Notes:** Large enough to ensure posting the large preimage is expensive enough to act as a deterrent but small enough
to be used for any preimage that is too large to be submitted in a single transaction.<br/>

### [Large Preimage Proposal Challenge Period](https://specs.optimism.io/fault-proof/stage-one/fault-dispute-game.html#preimageoracle-interaction)

**Description:** The amount of time that large preimage proposals can be challenged before they can be published to the
`PreimageOracle`<br/>
**Administrator:** Static<br/>
**Requirement:** 24 hours<br/>
**Notes:** High security. Allows enough time for honest actors to challenge invalid large preimage proposals.<br/>

### [Fault Game Absolute Prestate](https://specs.optimism.io/fault-proof/stage-one/fault-dispute-game.html#execution-trace)

**Description:** The VM state commitment to use as the starting point when executing the fault proof VM<br/>
**Administrator:** Static<br/>
**Requirement:** The state commitment of a governance approved op-program release.<br/>
**Notes:** The op-program version must have the rollup config and L2 genesis of the chain built in via the superchain
registry.<br/>

### [Fault Game Genesis Block](https://specs.optimism.io/fault-proof/stage-one/fault-dispute-game.html#anchor-state)

**Description:** The L2 block number used as the initial anchor state for fault dispute games<br/>
**Administrator:** Static<br/>
**Requirement:** Block number of any finalized block between bedrock activation and enabling fault proofs. 0 for chains
using fault proofs from genesis.<br/>

### [Fault Game Genesis Output Root](https://specs.optimism.io/fault-proof/stage-one/fault-dispute-game.html#anchor-state)

**Description:** The output root at the Fault Game Genesis Block<br/>
**Administrator:** Static<br/>
**Requirement:** The output root from the canonical chain at Fault game Genesis Block.<br/>

### [Fee Scalar](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L288-L294)

**Description:** Markup on transactions compared to the raw L1 data cost.<br/>
**Administrator:** [System Config Owner](#admin-roles)<br/>
**Requirement:** Set such that Fee Margin is between 0 and 50%.<br/>

### [Gas Limit](./system-config.md#gaslimit-uint64)

**Description:** Gas limit of the L2 blocks is configured through the system config.<br/>
**Administrator:** [System Config Owner](#admin-roles)<br/>
**Requirement:** No higher than 200_000_000 gas<br/>
**Notes:** Chain operators are driven to maintain a stable and reliable chain. When considering to change this value,
careful deliberation is necessary.<br/>

### Genesis state

**Description:** Initial state at chain genesis, including code and storage of predeploys (all L2 smart contracts).
See [Predeploy](../glossary.md#l2-genesis-block).<br/>
**Administrator:** Static<br/>
**Requirement:** Only standard predeploys and preinstalls, no additional state.<br/>
**Notes:** Homogeneity & standardization, ensures initial state is secure.<br/>

### [L2 block time](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/L2OutputOracle.sol#L105)

**Description:** Frequency with which blocks are produced as a result of derivation.<br/>
**Administrator:** [L1 Proxy Admin](#admin-roles)<br/>
**Requirement:** 2 seconds<br/>
**Notes:** High security & [interoperability](../interop/overview.md) compatibility requirement, until de-risked/solved
at app layer.<br/>

### [Resource config](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L338-L340)

**Description:** Config for the EIP-1559 based curve used for the deposit gas market.<br/>
**Administrator:** [L1 Proxy Admin](#admin-roles)<br/>
**Requirement:** See [resource config table](#resource-config).<br/>
**Notes:** Constraints are imposed
in [code](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L345-L365)
when setting the resource config.<br/>

### [Sequencing window Size](../glossary.md#sequencing-window)

**Description:** Maximum allowed batch submission gap, after which L1 fallback is triggered in derivation.<br/>
**Administrator:** Static<br/>
**Requirement:** 3_600 base layer blocks (12 hours for an L2 on Ethereum, assuming 12 second L1 blocktime). e.g. 12
second blocks, $3600 * 12\ seconds \div 60\frac{seconds}{minute} \div 60\frac{minute}{hour} = 12\ hours$.<br/>
**Notes:** This is an important value for constraining the sequencer's ability to re-order transactions; higher values
would pose a risk to user protections.<br/>

### [Start block](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L184)

**Description:** Block at which the system config was initialized the first time.<br/>
**Administrator:** [L1 Proxy Admin](#admin-roles)<br/>
**Requirement:** The block where the SystemConfig was initialized.<br/>
**Notes:** Simple clear restriction.<br/>

### [Superchain target](../protocol/superchain-upgrades.md#superchain-target)

**Description:** Choice of cross-L2 configuration. May be omitted in isolated OP Stack deployments. Includes
SuperchainConfig and ProtocolVersions contract addresses.<br/>
**Administrator:** Static<br/>
**Requirement:** Mainnet or Sepolia<br/>
**Notes:** A superchain target defines a set of layer 2 chains which share `SuperchainConfig` and `ProtocolVersions`
contracts deployed on layer 1.<br/>

### Governance Token

**Description:** OP token used for the Optimism Collective's Token House governance.<br/>
**Administrator:** n/a<br/>
**Requirement:** Disabled<br/>
**Notes:** Simple clear restriction.<br/>

[^chain-id-uniqueness]: The chain ID must be globally unique among all EVM chains.

### Resource Config

| Config Property             | Standard Config Requirement |
|-----------------------------|-----------------------------|
| maxResourceLimit            | $2*10^7$                    |
| elasticityMultiplier        | $10$                        |
| baseFeeMaxChangeDenominator | $8$                         |
| minimumBaseFee              | $1*10^9$                    |
| systemTxMaxGas              | $1*10^6$                    |
| maximumBaseFee              | $2^{128}$-1                 |

## Policy Parameters

### Data Availability Type

**Description:** Batcher can currently be configured to use blobs or calldata (
See [Data Availability Provider](../glossary.md#data-availability-provider)).<br/>
**Administrator:** [Batch submitter address](#service-roles)<br/>
**Requirement:** Ethereum (Blobs, Calldata)<br/>
**Notes:** Alt-DA is not yet supported for the standard configuration, but the sequencer can switch at-will between blob
and calldata with no restriction, since both are L1 security.<br/>

### Batch submission frequency

**Description:** Frequency with which batches are submitted to L1 (
see [Batcher Transaction](../glossary.md#batcher-transaction)).<br/>
**Administrator:** [Batch submitter address](#service-roles)<br/>
**Requirement:** 1_800 base layer blocks (6 hours for an L2 on Ethereum, assuming 12 second L1 blocktime) or lower<br/>
**Notes:** Batcher needs to fully submit each batch within the sequencing window, so this leaves buffer to account for
L1 network congestion and the amount of data the batcher would need to post.<br/>

### [Output frequency](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/L2OutputOracle.sol#L104)

**Description:** Frequency with which output roots are submitted to L1.<br/>
**Administrator:** [L1 Proxy Admin](#admin-roles)<br/>
**Requirement:** 43_200 L2 Blocks (24 hours for an L2 on Ethereum, assuming 2 second L2 blocktime) or lower e.g. 2
second blocks, $43200 * 2\ seconds \div 60\frac{seconds}{minute} \div 60\frac{minute}{hour} = 24\ hours$.<br/>
**Notes:** Deprecated once fault proofs are implemented. This value cannot be 0.<br/>

## Admin Roles

### L1 Proxy Admin

**Description:** Account authorized to upgrade L1 contracts.<br/>
**Administrator:** [L1 Proxy Admin Owner](#admin-roles)<br/>
**Administers:** [Batch Inbox Address](#consensus-parameters), [Start block](#consensus-parameters),
[Proposer address](#service-roles), [Challenger address](#service-roles), [Guardian address](#service-roles),
[Proof Maturity Delay](#consensus-parameters), [Dispute Game Finality](#consensus-parameters),
[Output frequency](#policy-parameters), [L2 block time](#consensus-parameters),
[L1 smart contracts](#consensus-parameters)<br/>
**Requirement:**
[ProxyAdmin.sol](https://github.com/ethereum-optimism/optimism/blob/op-contracts/v1.3.0/packages/contracts-bedrock/src/universal/ProxyAdmin.sol)
from the latest `op-contracts/vX.Y.X` release of source code in
[Optimism repository](https://github.com/ethereum-optimism/optimism).<br/>
**Notes:** Governance-controlled, high security.<br/>

### L1 ProxyAdmin owner

**Description:** Account authorized to update the L1 Proxy Admin.<br/>
**Administrator:** <br/>
**Administers:** [L1 Proxy Admin](#admin-roles)<br/>
**Requirement:**
[0x5a0Aae59D09fccBdDb6C6CcEB07B7279367C3d2A](https://etherscan.io/address/0x5a0Aae59D09fccBdDb6C6CcEB07B7279367C3d2A)
[^of-sc-gnosis-safe-l1]<br/>
**Notes:** Governance-controlled, high security.<br/>

### L2 Proxy Admin

**Description:** Account authorized to upgrade L2 contracts.<br/>
**Administrator:** [L2 Proxy Admin Owner](#admin-roles)<br/>
**Administers:** [Predeploys](./predeploys.md#overview)<br/>
**Requirement:**
[ProxyAdmin.sol](https://github.com/ethereum-optimism/optimism/blob/op-contracts/v1.3.0/packages/contracts-bedrock/src/universal/ProxyAdmin.sol)
from the latest `op-contracts/vX.Y.X` release of source code
in [Optimism repository](https://github.com/ethereum-optimism/optimism). Predeploy
address:  [0x4200000000000000000000000000000000000018](https://docs.optimism.io/chain/addresses#op-mainnet-l2).<br/>
**Notes:** Governance-controlled, high security.<br/>

### L2 ProxyAdmin owner

**Description:** Account authorized to upgrade protocol contracts via calls to the `ProxyAdmin`. This is the aliased L1
ProxyAdmin owner address.<br/>
**Administrator:** <br/>
**Administers:** [L2 Proxy Admin](#admin-roles)<br/>
**Requirement:** Gnosis Safe between Optimism Foundation (OF) and the Security Council (SC). Aliased
Address:
[0x6B1BAE59D09fCcbdDB6C6cceb07B7279367C4E3b](https://optimistic.etherscan.io/address/0x6B1BAE59D09fCcbdDB6C6cceb07B7279367C4E3b)
[^aliased-of-sc-gnosis-safe-l1]<br/>
**Notes:** Governance-controlled, high security.<br/>

### [System Config Owner](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L14C26-L14C44)

**Description:** Account authorized to change values in the SystemConfig contract. All configuration is stored on L1 and
picked up by L2 as part of the [derivation](./derivation.md) of the L2 chain.<br/>
**Administrator:** <br/>
**Administers:** [Batch submitter address](#service-roles), [Sequencer P2P / Unsafe head signer](#service-roles),
[Fee Margin](#consensus-parameters), [Gas limit](#consensus-parameters), [System Config Owner](#admin-roles)<br/>
**Requirement:** Chain Governor or Servicer<br/>
**Notes:** As defined in
the [Law of Chains](https://github.com/ethereum-optimism/OPerating-manual/blob/main/Law%20of%20Chains.md)<br/>

[^of-sc-gnosis-safe-l1]: 2 of 2 GnosisSafe between Optimism Foundation (OF) and the Security Council (SC) on L1. Mainnet and Sepolia addresses can be found at [privileged roles](https://docs.optimism.io/chain/security/privileged-roles#l1-proxy-admin).
[^aliased-of-sc-gnosis-safe-l1]: Aliased address of the 2 of 2 Gnosis Safe between Optimism Foundation (OF) and the Security Council (SC) on L1. The reason for aliasing can be found in the [glossary](../glossary.md#address-aliasing). This address was calculated using the following arithmetic: `0x5a0Aae59D09fccBdDb6C6CcEB07B7279367C3d2A` + `0x1111000000000000000000000000000000001111` = `0x6B1BAE59D09fCcbdDB6C6cceb07B7279367C4E3b`.

## Service Roles

### [Batch submitter address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L265)

**Description:** Account which authenticates new batches submitted to L1 Ethereum.<br/>
**Administrator:** [System Config Owner](#admin-roles)<br/>
**Requirement:** No requirement<br/>
**Notes:** <br/>

### [Challenger address](https://github.com/ethereum-optimism/optimism/blob/op-contracts/v1.5.0/packages/contracts-bedrock/src/dispute/PermissionedDisputeGame.sol#L23)

**Description:** Account which can interact with
existing [permissioned dispute games](../fault-proof/stage-one/bridge-integration.md#permissioned-faultdisputegame).<br/>
**Administrator:** [L1 Proxy Admin](#admin-roles)<br/>
**Requirement:**
[0x9BA6e03D8B90dE867373Db8cF1A58d2F7F006b3A](https://etherscan.io/address/0x9BA6e03D8B90dE867373Db8cF1A58d2F7F006b3A)
[^of-gnosis-safe-l1]<br/>
**Notes:** Optimism Foundation (OF) multisig
leveraging [battle-tested software](https://github.com/safe-global/safe-smart-account). This role is only active when
the `OptimismPortal` respected game type is [`PERMISSIONED_CANNON`]
(<https://github.com/ethereum-optimism/optimism/blob/op-contracts/v1.5.0/packages/contracts-bedrock/src/dispute/lib/Types.sol#L31>).<br/>

### [Guardian address](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SuperchainConfig.sol#L50)

**Description:** Account authorized to pause L1 withdrawals from contracts, blacklist dispute games, and set the
respected game type in the `OptimismPortal`.<br/>
**Administrator:** [L1 Proxy Admin](#admin-roles)<br/>
**Requirement:**
[0x09f7150D8c019BeF34450d6920f6B3608ceFdAf2](https://etherscan.io/address/0x09f7150D8c019BeF34450d6920f6B3608ceFdAf2)<br/>
**Notes:** A 1/1 Safe owned by the Security Council Safe, with
the [Deputy Guardian Module](../protocol/safe-extensions.md#deputy-guardian-module) enabled to allow the Optimism
Foundation to act as Guardian.<br/>

### [Proposer address](https://github.com/ethereum-optimism/optimism/blob/op-contracts/v1.5.0/packages/contracts-bedrock/src/dispute/PermissionedDisputeGame.sol#L20)

**Description:** Account which can create and interact
with [permissioned dispute games](../fault-proof/stage-one/bridge-integration.md#permissioned-faultdisputegame) on
L1.<br/>
**Administrator:** [L1 Proxy Admin](#admin-roles)<br/>
**Requirement:** No requirement<br/>
**Notes:** This role is only active when the `OptimismPortal` respected game type is [`PERMISSIONED_CANNON`]
(<https://github.com/ethereum-optimism/optimism/blob/op-contracts/v1.5.0/packages/contracts-bedrock/src/dispute/lib/Types.sol#L31>).
The `L1ProxyAdmin` sets the implementation of the `PERMISSIONED_CANNON` game type. Thus, it determines the proposer
configuration of the permissioned dispute game.<br/>

### [Sequencer P2P / Unsafe head signer](https://github.com/ethereum-optimism/optimism/blob/c927ed9e8af501fd330349607a2b09a876a9a1fb/packages/contracts-bedrock/src/L1/SystemConfig.sol#L250)

**Description:** Account which authenticates the unsafe/pre-submitted blocks for a chain at the P2P layer.<br/>
**Administrator:** [System Config Owner](#admin-roles)<br/>
**Requirement:** No requirement<br/>
**Notes:** <br/>

[^of-gnosis-safe-l1]: 5 of 7 GnosisSafe controlled by Optimism Foundation (OF). Mainnet and Sepolia addresses can be found at [privileged roles](https://docs.optimism.io/chain/security/privileged-roles).
