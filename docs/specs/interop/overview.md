<!-- DOCTOC SKIP -->

# Interop

The ability for a blockchain to easily read the state of another blockchain is called interoperability.
Low latency interoperability allows for horizontally scalable blockchains, a key feature of the superchain.

Note: this document references an "interop network upgrade" as a temporary name. The pending name of the
network upgrade is isthmus.

| Term                | Definition                                                                                          |
|---------------------|-----------------------------------------------------------------------------------------------------|
| Source Chain        | A blockchain that includes an initiating message                                                    |
| Destination Chain   | A blockchain that includes an executing message                                                     |
| Initiating Message  | An event emitted from a source chain                                                                |
| Identifier          | A unique pointer to an initiating message                                                           |
| Executing Message   | An event emitted from a destination chain's `CrossL2Inbox` that includes an initiating message and identifier |
| Cross Chain Message | The cumulative execution and side effects of the initiating message and executing message           |
| Dependency Set      | The set of chains that originate initiating transactions where the executing transactions are valid |

A total of two transactions are required to complete a cross chain message.
The first transaction is submitted to the source chain and emits an event (initiating message) that can be
consumed on a destination chain. The second transaction is submitted to the destination chain and includes the
initiating message as well as the identifier that uniquely points to the initiating message.

The chain's fork choice rule will reorg out any blocks that contain an executing message that is not valid,
meaning that the identifier actually points to the initiating message that was passed into the remote chain.
This means that the block builder SHOULD only include an executing message if they have already checked its validity.

The term "block builder" is used interchangeably with the term "sequencer" for the purposes of this document but
they need not be the same entity in practice.

## Specifications

- [Dependency Set](./dependency-set.md): definition of chains and chain-dependencies in the Superchain.
- [Messaging](./messaging.md): messaging functionality, core of protocol-level interoperability.
- [Predeploys](./predeploys.md): system contracts to interface with other chains.
- [Sequencer](./sequencer.md): Sequencer Policy and block-building information.
- [Verifier](./verifier.md): Verification of cross-L2 messaging.
- [Rollup Node P2P](./rollup-node-p2p.md): modifications to the rollup-node P2P layer to support fast interop.
- [Fault Proof](./fault-proof.md): modifications to prove interop functionality in the fault-proof.
- [Upgrade](./upgrade.md): Superchain upgrade process to activate Interop.
- [Token Bridging](./token-bridging.md): sending ERC20 tokens between chains
- [Superchain WETH](./superchain-weth.md): Making ETH interoperable.
- [Derivation](./derivation.md): Changes to derivation of block-attributes.
- [Transaction Pool](./tx-pool.md): Transaction-pool validation of transactions.
