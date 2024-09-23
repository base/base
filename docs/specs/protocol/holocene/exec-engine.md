# L2 Execution Engine

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [`L2ToL1MessagePasser` Storage Root in Header](#l2tol1messagepasser-storage-root-in-header)
  - [Timestamp Activation](#timestamp-activation)
  - [Header Validity Rules](#header-validity-rules)
  - [Header Withdrawals Root](#header-withdrawals-root)
    - [Rationale](#rationale)
    - [Forwards Compatibility Considerations](#forwards-compatibility-considerations)
    - [Client Implementation Considerations](#client-implementation-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## `L2ToL1MessagePasser` Storage Root in Header

After the Holocene hardfork's activation, the L2 block header's `withdrawalsRoot` field will consist of the 32-byte
[`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root _after_ the block has been executed, and _after_ the
insertions and deletions have been applied to the trie. In other words, the storage root should be the same root
that is returned by `eth_getProof` at the given block number.

### Timestamp Activation

Holocene, like other network upgrades, is activated at a timestamp.
Changes to the L2 Block execution rules are applied when the `L2 Timestamp >= activation time`.
Changes to the L2 block header are applied when it is considering data from a L1 Block whose timestamp
is greater than or equal to the activation timestamp.

### Header Validity Rules

Prior to holocene activation, the L2 block header's `withdrawalsRoot` field must be:

- `nil` if Canyon has not been activated.
- `keccak256(rlp(empty_string_code))` if Canyon has been activated.

After Holocene activation, an L2 block header's `withdrawalsRoot` field is valid iff:

1. It is exactly 32 bytes in length.
1. The [`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root, as committed to in the `storageRoot` within the block
   header, is equal to the header's `withdrawalsRoot` field.

### Header Withdrawals Root

| Byte offset | Description                                               |
| ----------- | --------------------------------------------------------- |
| `[0, 32)`   | [`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root |

#### Rationale

Currently, to generate [L2 output roots][output-root] for historical blocks, an archival node is required. This directly
places a burden on users of the system in a post-fault-proofs world, where:

1. A proposer must have an archive node to propose an output root at the safe head.
1. A user that is proving their withdrawal must have an archive node to verify that the output root they are proving
   their withdrawal against is indeed valid and included within the safe chain.

Placing the [`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root in the `withdrawalsRoot` field alleviates this burden
for users and protocol participants alike, allowing them to propose and verify other proposals with lower operating costs.

#### Forwards Compatibility Considerations

As it stands, the `withdrawalsRoot` field is unused within the OP Stack's header consensus format, and will never be
used for other reasons that are currently planned. Setting this value to the account storage root of the withdrawal
directly fits with the OP Stack, and makes use of the existing field in the L1 header consensus format.

#### Client Implementation Considerations

Varous EL clients store historical state of accounts differently. If, as a contrived case, an OP Stack chain did not have
an outbound withdrawal for a long period of time, the node may not have access to the account storage root of the
[`L2ToL1MessagePasser`][l2-to-l1-mp]. In this case, the client would be unable to keep consensus. However, most modern
clients are able to at the very least reconstruct the account storage root at a given block on the fly if it does not
directly store this information.

[l2-to-l1-mp]: ../../protocol/predeploys.md#L2ToL1MessagePasser
[output-root]: ../../glossary.md#l2-output-root
