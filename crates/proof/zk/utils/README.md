# base-proof-zk-utils

Guest-side STF helpers shared by zkVM range programs.

This crate must stay zkVM-buildable: no host RPC, tokio, or proving-backend
dependencies. SP1 guest I/O (`sp1_zkvm`) stays in the Succinct programs, not
here.

Host-side witness capture lives in `base-proof-zk-witness`.
