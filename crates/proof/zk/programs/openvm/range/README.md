# `range`

OpenVM client program for executing the Base rollup state transition across a
range of blocks. Same STF as the Succinct guest in
`crates/proof/zk/programs/succinct/range`: rkyv-encoded `DefaultWitnessData` on
stdin, `base-proof-zk-utils` derivation + execution, public values out.

The OpenVM public value is `keccak256(abi.encode(BootInfoStruct))` rather than
SP1's full committed struct, because OpenVM reveals a single `bytes32`.
