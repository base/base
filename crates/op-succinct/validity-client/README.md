# `validity-client`

This binary contains the client program for executing the Optimism rollup state transition across a range of blocks, which can be used to generate an on chain validity proof. Depending on the compilation pipeline, it will compile to be run on `kona-host` or on `zkvm-host` (in SP1).