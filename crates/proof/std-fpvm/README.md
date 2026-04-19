# `base-proof-std-fpvm`

Platform specific [Fault Proof VM][g-fault-proof-vm] kernel APIs.

## Overview

Provides low-level kernel abstractions for FPVM-targeted client programs (MIPS64 and RISC-V).
Exposes the `BasicKernelInterface` trait for I/O, memory allocation, and system calls, along with
`FileDescriptor`, `FileChannel`, and architecture-specific implementations. Used by fault proof
client programs to read preimages from the oracle and write hints to the host.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-std-fpvm = { workspace = true }
```

```rust,ignore
use base_proof_std_fpvm::{BasicKernelInterface, FileDescriptor};

let fd = FileDescriptor::StdIn;
let mut buf = [0u8; 32];
BasicKernelInterface::read(fd, &mut buf)?;
```

[g-fault-proof-vm]: https://specs.base.org/protocol/fault-proof#fault-proof-vm

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
