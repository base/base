# Verifier

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Driver](#driver)
  - [Safety](#safety)
    - [`unsafe` Inputs](#unsafe-inputs)
    - [`cross-unsafe` Inputs](#cross-unsafe-inputs)
    - [`safe` Inputs](#safe-inputs)
    - [`finalized` Inputs](#finalized-inputs)
  - [Honest Verifier](#honest-verifier)
- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Driver

The driver is responsible for validating L2 blocks and promoting them from unsafe
to safe. A series of invariants are enforced before promotion.

### Safety

Safety is an abstraction that is useful for reasoning about the security of L2 blocks.
It is a spectrum from `unsafe` to `finalized`. Users can choose to operate on L2 data
based on its level of safety taking into account their risk profile and personal preferences.

The following labels are used to describe both inputs and outputs:

- `unsafe`
- `cross-unsafe`
- `safe`
- `finalized`

Inputs correspond to the inputs to the state transition function while outputs correspond to the side
effects of the state transition function.

Anything before `safe` technically uses a "preconfirmation" based security model which is not part
of consensus. While useful to have definitions of the default meanings of these terms, they are
technically policy and may be changed in the future.

The `unsafe` label has the lowest latency while the `finalized` label has the highest latency.
A set of invariants must be held true before an input or an output can be promoted to the next
label, starting at `unsafe`.

The initiating messages for all dependent executing messages MUST be resolved as safe before an L2 block can transition
from being unsafe to safe. Users MAY optimistically accept unsafe blocks without any verification of the
executing messages. They SHOULD optimistically verify the initiating messages exist in destination unsafe blocks
to more quickly reorganize out invalid blocks.

#### `unsafe` Inputs

- MUST be signed by the p2p sequencer key
- MAY be reorganized
- MUST be promoted to a higher level of safety or reorganized out to ensure liveness

`unsafe` inputs are currently gossiped around the p2p network. To prevent denial of service, they MUST
be signed by the sequencer. This signature represents the sequencer's claim that it
built a block that conforms to the protocol. `unsafe` blocks exist to give low latency access to the
latest information. To keep the latency as low as possible, cross chain messages are assumed valid
at this stage. This means that the remote unsafe inputs are trusted solely because they were
included in a block by the sequencer.

An alternative approach to `unsafe` inputs would be to include an SGX proof that the sequencer ran
particular software when building the block.

#### `cross-unsafe` Inputs

- MUST have valid cross chain messages

`cross-unsafe` represents the `unsafe` blocks that had their cross chain messages fully verified.
The network can be represented as a graph where each block across all chains are represented as
a node and then a directed edge between two blocks represents the source block of the initiating
message and the block that included the executing message.

An input can be promoted from `unsafe` to `cross-unsafe` when the full dependency graph is resolved
such that all cross chain messages are verified to be valid and at least one message in the dependency
graph is still `unsafe`.

Note that the `cross-unsafe` is not meant to be exposed to the end user via the RPC label. It is
meant for internal usage. All `cross-unsafe` inputs are still considered `unsafe` by the execution
layer RPC.

#### `safe` Inputs

- MUST be available
- MAY be reorganized
- Safe block MUST be invalidated if a reorg occurs

`safe` represents the state in which the `cross-unsafe` dependency graph has been fully resolved
in a way where all of the data has been published to the data availability layer.

#### `finalized` Inputs

- MUST NOT be reorganized based on Ethereum economic security

`finalized` represents full Proof of Stake economic security on top of the data. This means that
if the data is reorganized, then validators will be slashed.

### Honest Verifier

The honest verifier follows a naive verification algorithm. That is similar
to the block building code that the [sequencer](./sequencer.md#direct-dependency-confirmation)
follows. The main difference is that the validity of included executing
messages is verified instead of verifying possible executing messages before
inclusion.

## Security Considerations
