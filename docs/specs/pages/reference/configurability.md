# Configuration

There are four categories of Base configuration:

- **Consensus Parameters**: Fixed at genesis or changeable through privileged accounts or protocol upgrades.
- **Policy Parameters**: Changeable without breaking consensus, within protocol-imposed constraints.
- **Admin Roles**: Accounts that can upgrade contracts, change role owners, or update protocol parameters. Typically cold/multisig wallets.
- **Service Roles**: Accounts used for day-to-day operations. Typically hot wallets.

## Consensus Parameters

| Parameter | Description | Administrator |
|-----------|-------------|---------------|
| [Batch Inbox Address](glossary.md#batch-inbox) | L1 address where [batcher transactions](glossary.md#batcher-transaction) are posted | Static |
| [Batcher Hash](glossary.md#batcher-hash) | Versioned hash of the authorized batcher sender(s) | [System Config Owner](#admin-roles) |
| Chain ID | Unique chain ID for transaction signature validation | Static |
| [Proof Maturity Delay](../protocol/fault-proof/stage-one/bridge-integration.md#fpac-optimismportal-mods-specification) | Time between proving and finalizing a withdrawal. 7 days. | [L1 Proxy Admin](#admin-roles) |
| [Dispute Game Finality](../protocol/fault-proof/stage-one/bridge-integration.md#fpac-optimismportal-mods-specification) | Time for `Guardian` to [blacklist a game](../protocol/fault-proof/stage-one/bridge-integration.md#blacklisting-disputegames) before withdrawals finalize. 3.5 days. | [L1 Proxy Admin](#admin-roles) |
| [Respected Game Type](../protocol/fault-proof/stage-one/bridge-integration.md#new-state-variables) | Game type `OptimismPortal` accepts for withdrawal finalization. `CANNON` (`0`); may fall back to `PERMISSIONED_CANNON` (`1`). | [Guardian](#service-roles) |
| [Fault Game Max Depth](../protocol/fault-proof/stage-one/fault-dispute-game.md#game-tree) | Maximum depth of fault dispute game trees. 73. | Static |
| [Fault Game Split Depth](../protocol/fault-proof/stage-one/fault-dispute-game.md#game-tree) | Depth after which claims correspond to VM state commitments. 30. | Static |
| [Max Game Clock Duration](../protocol/fault-proof/stage-one/fault-dispute-game.md#max_clock_duration) | Maximum time on a dispute game team's chess clock. 3.5 days. | Static |
| [Game Clock Extension](../protocol/fault-proof/stage-one/fault-dispute-game.md#clock_extension) | Clock credit when a team's remaining time falls below `CLOCK_EXTENSION`. 3 hours. | Static |
| [Bond Withdrawal Delay](../protocol/fault-proof/stage-one/bond-incentives.md#delay-period) | Time before dispute game bonds can be withdrawn. 7 days. | Static |
| [Min Large Preimage Size](../protocol/fault-proof/stage-one/fault-dispute-game.md#preimageoracle-interaction) | Minimum preimage size for the `PreimageOracle` large proposal process. 126,000 bytes. | Static |
| [Large Preimage Challenge Period](../protocol/fault-proof/stage-one/fault-dispute-game.md#preimageoracle-interaction) | Challenge window before large preimage proposals are published. 24 hours. | Static |
| [Fault Game Absolute Prestate](../protocol/fault-proof/stage-one/fault-dispute-game.md#execution-trace) | VM state commitment used as the fault proof VM starting point | Static |
| [Fault Game Genesis Block](../protocol/fault-proof/stage-one/fault-dispute-game.md#anchor-state) | Initial [anchor state](../protocol/fault-proof/stage-one/fault-dispute-game.md#anchor-state) block number. Any finalized block between bedrock and fault proof activation; `0` from genesis. | Static |
| [Fault Game Genesis Output Root](../protocol/fault-proof/stage-one/fault-dispute-game.md#anchor-state) | Output root at the Fault Game Genesis Block | Static |
| [Fee Scalar](glossary.md#fee-scalars) | Markup on transactions relative to raw L1 data cost. Fee margin between 0%–50%. | [System Config Owner](#admin-roles) |
| [Gas Limit](../protocol/consensus/derivation.md#system-configuration) | L2 block gas limit. ≤ 200,000,000 gas. | [System Config Owner](#admin-roles) |
| [Genesis State](../protocol/execution/evm/predeploys.md#overview) | Initial chain state including all predeploy code and storage. Standard predeploys and preinstalls only. | Static |
| L2 Block Time | Interval at which L2 blocks are produced via [derivation](../protocol/consensus/derivation.md). 1 or 2 seconds. | [L1 Proxy Admin](#admin-roles) |
| [Sequencing Window Size](glossary.md#sequencing-window) | Max batch submission gap before L1 fallback triggers. 3,600 L1 blocks (12 hours at 12s L1 block time). | Static |
| Start Block | L1 block where `SystemConfig` was first initialized | [L1 Proxy Admin](#admin-roles) |
| Superchain Target | `SuperchainConfig` and `ProtocolVersions` addresses for cross-L2 config. Mainnet or Sepolia. | Static |
| Governance Token | OP governance token. Disabled. | n/a |
| [Operator Fee Params](../upgrades/isthmus/exec-engine.md#operator-fee) | Operator fee scalar and constant for fee calculation. Standard values are 0; non-zero for non-standard configurations such as op-succinct. | [System Config Owner](#admin-roles) |
| [DA Footprint Gas Scalar](../upgrades/jovian/exec-engine.md#DA-footprint-block-limit) | Scalar for DA footprint calculation | [System Config Owner](#admin-roles) |
| [Minimum Base Fee](../upgrades/jovian/exec-engine.md#minimum-base-fee) | Minimum base fee on L2 | [System Config Owner](#admin-roles) |

## Policy Parameters

| Parameter | Description | Administrator |
|-----------|-------------|---------------|
| [Data Availability Type](glossary.md#data-availability-provider) | Whether the batcher posts data as blobs or calldata. Ethereum (Blobs or Calldata); Alt-DA not supported. | [Batch Submitter](#service-roles) |
| Batch Submission Frequency | Frequency of [batcher transaction](glossary.md#batcher-transaction) submissions to L1. ≤ 1,800 L1 blocks (6 hours at 12s L1 block time). | [Batch Submitter](#service-roles) |
| Output Frequency | Frequency of output root submissions to L1. ≤ 43,200 L2 blocks (24 hours at 2s L2 block time); must be non-zero. Deprecated once fault proofs are enabled. | [L1 Proxy Admin](#admin-roles) |

## Admin Roles

| Role | Description | Administers |
|------|-------------|-------------|
| L1 Proxy Admin | `ProxyAdmin` from the latest `op-contracts` release, authorized to upgrade L1 contracts | L1 contracts |
| L1 ProxyAdmin Owner | Authorized to update the L1 Proxy Admin. [0x5a0Aae59D09fccBdDb6C6CcEB07B7279367C3d2A](https://etherscan.io/address/0x5a0Aae59D09fccBdDb6C6CcEB07B7279367C3d2A) | [L1 Proxy Admin](#admin-roles) |
| L2 Proxy Admin | `ProxyAdmin` at `0x4200000000000000000000000000000000000018`, authorized to upgrade L2 contracts | [Predeploys](../protocol/execution/evm/predeploys.md#overview) |
| L2 ProxyAdmin Owner | [Aliased](glossary.md#address-aliasing) L1 ProxyAdmin Owner; upgrades L2 contracts via `ProxyAdmin`. [0x6B1BAE59D09fCcbdDB6C6cceb07B7279367C4E3b](https://optimistic.etherscan.io/address/0x6B1BAE59D09fCcbdDB6C6cceb07B7279367C4E3b) | [L2 Proxy Admin](#admin-roles) |
| [System Config Owner](../protocol/consensus/derivation.md#system-configuration) | Authorized to change values in the `SystemConfig` contract | [Batch Submitter](#service-roles), [Sequencer P2P Signer](#service-roles), Fee Scalar, Gas Limit |

## Service Roles

| Role | Description | Administrator |
|------|-------------|---------------|
| [Batch Submitter](glossary.md#batcher) | Authenticates batches submitted to L1 | [System Config Owner](#admin-roles) |
| [Challenger](../protocol/fault-proof/stage-one/bridge-integration.md#permissioned-faultdisputegame) | Interacts with permissioned dispute games. Active only when respected game type is `PERMISSIONED_CANNON`. [0x9BA6e03D8B90dE867373Db8cF1A58d2F7F006b3A](https://etherscan.io/address/0x9BA6e03D8B90dE867373Db8cF1A58d2F7F006b3A) | [L1 Proxy Admin](#admin-roles) |
| [Guardian](../protocol/stage-1.md#pause-mechanism) | Pauses L1 withdrawals, blacklists dispute games, sets respected game type in `OptimismPortal`. 1/1 Safe with [Deputy Pause Module](../protocol/deputy-pause-module.md) enabled. [0x09f7150D8c019BeF34450d6920f6B3608ceFdAf2](https://etherscan.io/address/0x09f7150D8c019BeF34450d6920f6B3608ceFdAf2) | [L1 Proxy Admin](#admin-roles) |
| [Proposer](../protocol/fault-proof/stage-one/bridge-integration.md#permissioned-faultdisputegame) | Creates permissioned dispute games on L1. Active only when respected game type is `PERMISSIONED_CANNON`. | [L1 Proxy Admin](#admin-roles) |
| [Sequencer P2P Signer](glossary.md#unsafe-block-signer) | Signs unsafe/pre-submitted blocks at the P2P layer | [System Config Owner](#admin-roles) |
