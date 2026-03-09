# `base-proof-std-fpvm-proc`

Proc macro entry point for `base-proof-std-fpvm` targeted programs.

## Overview

Provides the `#[client_entry]` procedural macro attribute that wraps a fault proof client
program's entry function with proper FPVM initialization: heap allocator setup, panic handler
registration, and exit code management. This macro is the standard way to define the entry
point of FPVM client programs.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-std-fpvm-proc = { workspace = true }
```

```rust,ignore
use base_proof_std_fpvm_proc::client_entry;

#[client_entry]
pub fn main() {
    // fault proof client logic
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
