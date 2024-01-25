# Execution

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

This is an experimental section and may be changed in the future. It is not required
for the initial release.

After the full execution of a block, the set of [logs][log] that are emitted MUST be merklized and
included in the block header. This commitment MUST be included as the block header's
[extra data field][block-extra-data]. The events are serialized with using [Simple Serialize][ssz] aka SSZ.

The goal of committing to data in the extra data field is to make commitments to data required to
verify executing messages easier. The commitment could also commit to the set of executing messages
instead of just the logs which represent all possible initiating messages that originate in the block.
This would reduce rounds of communication for nodes that use proofs to verify executing messages.

[log]: https://github.com/ethereum/go-ethereum/blob/5c67066a050e3924e1c663317fd8051bc8d34f43/core/types/log.go#L29
[block-extra-data]: https://github.com/ethereum/execution-specs/blob/1fed0c0074f9d6aab3861057e1924411948dc50b/src/ethereum/frontier/fork_types.py#L115
[ssz]: https://github.com/ethereum/consensus-specs/blob/dev/ssz/simple-serialize.md

| Name                     | Value                         |
|--------------------------|-------------------------------|
| `MAX_LOG_DATA_SIZE`      | `uint64(2**24)` or 16,777,216 |
| `MAX_MESSAGES_PER_BLOCK` | `uint64(2**20)` or 1,048,576  |

```python
class Log(Container):
    address: ExecutionAddress
    topics: List[bytes32, 4]
    data: ByteList[MAX_LOG_DATA_SIZE]
    transaction_hash: bytes32
    transaction_index: uint64
    log_index: uint64

logs = List[Log, MAX_MESSAGES_PER_BLOCK](
  log_0, log_1, log_2, ...)

block.extra_data = logs.hash_tree_root()
```

## Security Considerations

TODO
