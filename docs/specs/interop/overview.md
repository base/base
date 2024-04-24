<!-- DOCTOC SKIP -->

# Interop

The ability for a blockchain to easily read the state of another blockchain is called interoperability.
Low latency interoperability allows for horizontally scalable blockchains, a key feature of the superchain.

Note: this document references an "interop network upgrade" as a temporary name. The actual name of the
network upgrade will be included in this document in the future.

| Term                | Definition                                                                                          |
|---------------------|-----------------------------------------------------------------------------------------------------|
| Source Chain        | A blockchain that includes an initiating message                                                    |
| Destination Chain   | A blockchain that includes an executing message                                                     |
| Initiating Message  | An event emitted from a source chain                                                                |
| Executing Message   | An event emitted from a destination chain that includes an initiating message                       |
| Cross Chain Message | The cumulative execution and side effects of the initiating message and executing message           |
| Dependency Set      | The set of chains that originate initiating transactions where the executing transactions are valid |

A total of two transactions are required to complete a cross chain message.
The first transaction is submitted to the source chain and emits an event that can be consumed on a destination chain.
The second transaction is submitted to a destination chain, where the block builder SHOULD only include it if they are
certain that the first transaction was included in the source chain.
There is no strict requirement that the executing message is ever submitted.

The term "block builder" is used interchangeably with the term "sequencer" for the purposes of this document but
they need not be the same entity in practice.

## Specifications

- [Dependency Set](./dependency_set.md): definition of chains and chain-dependencies in the Superchain.
- [Messaging](./messaging.md): messaging functionality, core of protocol-level interoperability.
- [Predeploys](./predeploys.md): system contracts to interface with other chains.
- [Execution](./execution.md): enhancements to the execution-layer.
- [Sequencer](./sequencer.md): Sequencer Policy and block-building information.
- [Verifier](./verifier.md): Verification of cross-L2 messaging.
- [Rollup Node P2P](./rollup_node_p2p.md): modifications to the rollup-node P2P layer to support fast interop.
- [Fault Proof](./fault_proof.md): modifications to prove interop functionality in the fault-proof.
- [Upgrade](./upgrade.md): Superchain upgrade process to activate Interop.
