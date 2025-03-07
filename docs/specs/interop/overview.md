<!-- DOCTOC SKIP -->

# Interop

The ability for a blockchain to easily read the state of another blockchain is called interoperability.
Relatively trustless interop is possible between rollups by using L1 Ethereum as a hub. A message is
withdrawn from one chain to L1 and then deposited to another chain. The goal of OP Stack native interop
is to enable cross chain messaging at a much lower latency than going through L1. Low latency interoperability
allows for a horizontally scalable blockchain network.

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
| Log                 | The Ethereum consensus object created by the `LOG*` opcodes                                         |
| Event               | The solidity representation of a log                                                                |

A total of two transactions are required to complete a cross chain message.
The first transaction is submitted to the source chain and any log that is emitted can be
used as an initiating message that can be consumed on a destination chain. The second
transaction is submitted to the destination chain and includes the
initiating message as well as the identifier that uniquely points to the initiating message.

The chain's fork choice rule will reorg out any blocks that contain an executing message that is not valid.
A valid executing message means that the identifier correctly references its initiating message.
This means that the sequencer SHOULD only include an executing message if they have checked its validity.
The integrity of a message is guaranteed at the application layer without the need for any sort of confirmation
depth.

The proof system is able to check the validity of all executing messages.

## Specifications

- [Dependency Set](./dependency-set.md): definition of chains and chain-dependencies in the Superchain.
- [Messaging](./messaging.md): messaging functionality, core of protocol-level interoperability.
- [Predeploys](./predeploys.md): system contracts to interface with other chains.
- [Sequencer](./sequencer.md): Sequencer Policy and block-building information.
- [Verifier](./verifier.md): Verification of cross-L2 messaging.
- [Supervisor](./supervisor.md): API for validating messages.
- [Fault Proof](./fault-proof.md): modifications to prove interop functionality in the fault-proof.
- [Upgrade](./upgrade.md): Superchain upgrade process to activate Interop.
- [Token Bridging](./token-bridging.md): sending ERC20 tokens between chains
- [Superchain WETH](./superchain-weth.md): Making ETH interoperable.
- [Derivation](./derivation.md): Changes to derivation of block-attributes.
- [Transaction Pool](./tx-pool.md): Transaction-pool validation of transactions.
