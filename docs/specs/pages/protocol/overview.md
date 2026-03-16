# Overview

Base is a rollup built on Ethereum. L2 transaction data is posted to Ethereum for data availability,
and proofs allow anyone to challenge invalid state transitions. This page gives a high-level tour of the
protocol components and the core user flows.

## Network Participants

There are three primary actors that interact with Base: users, sequencers, and validators.

```mermaid
graph TD
    EthereumL1(Ethereum L1)

    subgraph "L2 Participants"
        Users(Users)
        Sequencers(Sequencers)
        Validators(Validators)
    end

    Validators -.->|fetch transaction batches| EthereumL1
    Validators -.->|fetch deposit data| EthereumL1
    Validators -->|submit/validate/challenge output proposals| EthereumL1
    Validators -.->|fetch realtime P2P updates| Sequencers

    Users -->|submit deposits/withdrawals| EthereumL1
    Users -->|submit transactions| Sequencers
    Users -->|query data| Validators

    Sequencers -->|submit transaction batches| EthereumL1
    Sequencers -.->|fetch deposit data| EthereumL1

    classDef l1Contracts stroke:#bbf,stroke-width:2px;
    classDef l2Components stroke:#333,stroke-width:2px;
    classDef systemUser stroke:#f9a,stroke-width:2px;

    class EthereumL1 l1Contracts;
    class Users,Sequencers,Validators l2Components;
```

### Users

Users are the general class of network participants who:

- Submit transactions through the sequencer or by interacting with contracts on Ethereum.
- Query transaction data from interfaces operated by validators.

### Sequencers

The sequencer fills the role of block producer on Base. Base currently operates with a single active sequencer.

The Sequencer:

- Accepts transactions directly from Users.
- Observes "deposit" transactions generated on Ethereum.
- Consolidates both transaction streams into ordered L2 blocks.
- Submits information to L1 that is sufficient to fully reproduce those L2 blocks.
- Provides real-time access to pending L2 blocks that have not yet been confirmed on L1.
- Produces Flashblocks every 200ms, committing to the ordering of transactions within the block as it is being built.

The Sequencer serves an important role for the operation of an L2 chain but is not a trusted actor. The Sequencer is generally
responsible for improving the user experience by ordering transactions much more quickly and cheaply than would currently
be possible if users were to submit all transactions directly to L1.

### Validators

Validators execute the L2 state transition function independently of the Sequencer. Validators help to maintain
the integrity of the network and serve blockchain data to Users.

Validators generally:

- Sync rollup data from L1 and the Sequencer.
- Use rollup data to execute the L2 state transition function.
- Serve rollup data and computed L2 state information to Users.

Validators can also act as Proposers and/or Challengers who:

- Submit assertions about the state of the L2 to a smart contract on L1.
- Validate assertions made by other participants.
- Dispute invalid assertions made by other participants.

## High-Level System Diagram

The following diagram shows how the major protocol components interact across L1 and L2.

```mermaid
graph LR
    subgraph "Ethereum L1"
        OptimismPortal(<a href="./bridging/withdrawals.html#the-optimism-portal-contract">OptimismPortal</a>)
        BatchInbox(<a href="../reference/glossary.html#batcher-transaction">Batch Inbox Address</a>)
        DisputeGameFactory(<a href="./fault-proof/stage-one/dispute-game-interface.html#disputegamefactory-interface">DisputeGameFactory</a>)
    end

    subgraph "L2 Node"
        RollupNode(<a href="./consensus/">Consensus</a>)
        ExecutionEngine(<a href="./execution/">Execution Engine</a>)
    end

    Batcher(<a href="./batcher.html">Batcher</a>)
    Proposers(Proposers)
    Challengers(Challengers)
    Users(Users)

    Users -->|deposits / withdrawals| OptimismPortal
    Users -->|transactions| ExecutionEngine

    Batcher -->|post transaction batches| BatchInbox
    Batcher -.->|fetch batch data| RollupNode

    RollupNode -.->|fetch batches| BatchInbox
    RollupNode -.->|fetch deposit events| OptimismPortal
    RollupNode -->|Engine API| ExecutionEngine

    Proposers -->|submit output proposals| DisputeGameFactory
    Proposers -.->|fetch outputs| RollupNode
    Challengers -->|verify / challenge games| DisputeGameFactory
    OptimismPortal -.->|query state proposals| DisputeGameFactory

    classDef l1Contracts stroke:#bbf,stroke-width:2px;
    classDef l2Components stroke:#333,stroke-width:2px;
    classDef systemUser stroke:#f9a,stroke-width:2px;

    class OptimismPortal,BatchInbox,DisputeGameFactory l1Contracts;
    class RollupNode,ExecutionEngine l2Components;
    class Batcher,Proposers,Challengers,Users systemUser;
```

## Protocol Components

### Consensus

Consensus is responsible for deriving the canonical L2 chain from L1 data. It reads transaction batches
from the Batch Inbox and deposit events from OptimismPortal, constructs payload attributes, and drives the
execution engine via the Engine API. Unsafe (unconfirmed) blocks are gossiped to other nodes over a dedicated
P2P network to give validators low-latency access before batches land on L1.

[Consensus →](./consensus/)

```mermaid
graph LR
    L1(Ethereum L1)
    subgraph "Rollup Node"
        BatchDecoding(Batch Decoding)
        Derivation(Derivation Pipeline)
    end
    EngineAPI(Engine API)
    EE(Execution Engine)
    L2(L2 Blocks)

    L1 -->|batches + deposit events| BatchDecoding
    BatchDecoding --> Derivation
    Derivation -->|payload attributes| EngineAPI
    EngineAPI --> EE
    EE --> L2

    classDef l1 stroke:#bbf,stroke-width:2px;
    classDef l2 stroke:#333,stroke-width:2px;
    class L1 l1;
    class EE,L2 l2;
```

### Execution

The execution engine is a Reth-based runtime. It exposes the standard Ethereum JSON-RPC API and
processes blocks produced by consensus. Predeploys (system contracts at fixed L2 addresses), precompiles,
and preinstalls extend the EVM for rollup-specific functionality such as fee distribution, L1 block attribute
injection, and cross-domain messaging.

[Execution →](./execution/)

### Bridging

Deposits flow from the `OptimismPortal` contract on L1 into L2 as special deposit transactions included at the
start of each L2 block. Withdrawals flow in the opposite direction: a withdrawal transaction is initiated on L2,
a proposer submits an output root to `DisputeGameFactory`, and after the challenge period the user proves and
finalizes the withdrawal on L1 via `OptimismPortal`.

[Bridging →](./bridging/deposits)

```mermaid
graph LR
    subgraph "Deposit Path"
        User1(User)
        OP1(OptimismPortal)
        DepTx(Deposit Transaction on L2)
    end

    subgraph "Withdrawal Path"
        User2(User)
        WdTx(Withdrawal Tx on L2)
        DGF(DisputeGameFactory)
        OP2(OptimismPortal)
    end

    User1 -->|depositTransaction| OP1
    OP1 -->|TransactionDeposited event| DepTx

    User2 -->|initiates withdrawal| WdTx
    WdTx -->|output root proposed| DGF
    User2 -->|prove + finalize| OP2
    OP2 -.->|verify game| DGF

    classDef l1 stroke:#bbf,stroke-width:2px;
    classDef systemUser stroke:#f9a,stroke-width:2px;
    class OP1,OP2,DGF l1;
    class User1,User2 systemUser;
```

### Batcher

The batcher is a service run by the sequencer that compresses L2 transaction data into channel frames and posts
them as calldata (or blobs) to the Batch Inbox Address on L1. This is the data availability layer that allows
any validator to independently reconstruct the L2 chain from L1.

[Batcher →](./batcher)

```mermaid
graph LR
    Sequencer(Sequencer)
    Batcher(<a href="./batcher.html">Batcher</a>)
    BatchInbox(<a href="../reference/glossary.html#batcher-transaction">Batch Inbox Address</a>)
    RollupNode(<a href="./consensus/">Rollup Node</a>)

    Sequencer -->|L2 blocks| Batcher
    Batcher -->|compressed channel frames| BatchInbox
    BatchInbox -.->|fetch batches| RollupNode

    classDef l1 stroke:#bbf,stroke-width:2px;
    classDef l2 stroke:#333,stroke-width:2px;
    classDef systemUser stroke:#f9a,stroke-width:2px;
    class BatchInbox l1;
    class RollupNode l2;
    class Batcher,Sequencer systemUser;
```

### Proofs

Output proposals and fault proofs allow permissionless verification of the L2 state. Anyone can propose an
output root to the `DisputeGameFactory`, and anyone can challenge it. Disputes are resolved by the `FaultDisputeGame`
contract using the Cannon VM for on-chain execution tracing of disputed state transitions. Valid withdrawals can
only be finalized through `OptimismPortal` once the associated dispute game resolves in favor of the proposer.

[Proofs →](./fault-proof/)

```mermaid
graph LR
    Proposer(Proposer)
    DGF(<a href="./fault-proof/stage-one/dispute-game-interface.html#disputegamefactory-interface">DisputeGameFactory</a>)
    FDG(<a href="./fault-proof/stage-one/fault-dispute-game.html">FaultDisputeGame</a>)
    Challengers(Challengers)
    OP(<a href="./bridging/withdrawals.html#the-optimism-portal-contract">OptimismPortal</a>)

    Proposer -->|submit output root| DGF
    DGF -->|create game| FDG
    Challengers -->|challenge / defend| FDG
    FDG -->|resolved result| OP

    classDef l1 stroke:#bbf,stroke-width:2px;
    classDef systemUser stroke:#f9a,stroke-width:2px;
    class DGF,FDG,OP l1;
    class Proposer,Challengers systemUser;
```

## Core User Flows

### Depositing ETH to Base

Users will often begin their L2 journey by depositing ETH from L1.
Once they have ETH to pay fees, they'll start sending transactions on L2.
The following diagram demonstrates this interaction and key Base protocol components.

```mermaid
graph TD
    subgraph "Ethereum L1"
        OptimismPortal(<a href="./bridging/withdrawals.html#the-optimism-portal-contract">OptimismPortal</a>)
        BatchInbox(<a href="../reference/glossary.html#batcher-transaction">Batch Inbox Address</a>)
    end

    Sequencer(Sequencer)
    Users(Users)

    %% Interactions
    Users -->|<b>1.</b> submit deposit| OptimismPortal
    Sequencer -.->|<b>2.</b> fetch deposit events| OptimismPortal
    Sequencer -->|<b>3.</b> generate deposit block| Sequencer
    Users -->|<b>4.</b> send transactions| Sequencer
    Sequencer -->|<b>5.</b> submit transaction batches| BatchInbox

    classDef l1Contracts stroke:#bbf,stroke-width:2px;
    classDef l2Components stroke:#333,stroke-width:2px;
    classDef systemUser stroke:#f9a,stroke-width:2px;

    class OptimismPortal,BatchInbox l1Contracts;
    class Sequencer l2Components;
    class Users systemUser;
```

### Sending Transactions on Base

Sending transactions on Base works the same as on Ethereum. Users sign transactions and submit them via
`eth_sendRawTransaction` to any node's JSON-RPC endpoint. The sequencer picks them up from its mempool,
orders them into L2 blocks, and eventually posts the batch to L1.

### Withdrawing from Base

Users may also want to withdraw ETH or ERC20 tokens from Base back to Ethereum. Withdrawals are initiated
as standard transactions on L2 but are then completed using transactions on L1. Withdrawals must reference a valid
`FaultDisputeGame` contract that proposes the state of the L2 at a given point in time.

```mermaid
graph LR
    subgraph "Ethereum L1"
        BatchInbox(<a href="../reference/glossary.html#batcher-transaction">Batch Inbox Address</a>)
        DisputeGameFactory(<a href="./fault-proof/stage-one/dispute-game-interface.html#disputegamefactory-interface">DisputeGameFactory</a>)
        FaultDisputeGame(<a href="./fault-proof/stage-one/fault-dispute-game.html">FaultDisputeGame</a>)
        OptimismPortal(<a href="./bridging/withdrawals.html#the-optimism-portal-contract">OptimismPortal</a>)
        ExternalContracts(External Contracts)
    end

    Sequencer(Sequencer)
    Proposers(Proposers)
    Users(Users)

    %% Interactions
    Users -->|<b>1.</b> send withdrawal initialization txn| Sequencer
    Sequencer -->|<b>2.</b> submit transaction batch| BatchInbox
    Proposers -->|<b>3.</b> submit output proposal| DisputeGameFactory
    DisputeGameFactory -->|<b>4.</b> generate game| FaultDisputeGame
    Users -->|<b>5.</b> submit withdrawal proof| OptimismPortal
    Users -->|<b>6.</b> wait for finalization| FaultDisputeGame
    Users -->|<b>7.</b> submit withdrawal finalization| OptimismPortal
    OptimismPortal -->|<b>8.</b> check game validity| FaultDisputeGame
    OptimismPortal -->|<b>9.</b> execute withdrawal transaction| ExternalContracts

    %% Styling
    classDef l1Contracts stroke:#bbf,stroke-width:2px;
    classDef l2Components stroke:#333,stroke-width:2px;
    classDef systemUser stroke:#f9a,stroke-width:2px;

    class BatchInbox,DisputeGameFactory,FaultDisputeGame,OptimismPortal l1Contracts;
    class Sequencer l2Components;
    class Users,Proposers systemUser;
```
