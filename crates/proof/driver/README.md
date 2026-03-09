# `base-proof-driver`

A `no_std` derivation pipeline driver.

## Overview

Orchestrates the block derivation and execution loop for proof generation. The `Driver` steps
through the derivation pipeline (`DriverPipeline`), manages resets and reorgs, and drives an
`Executor` to produce and execute payload attributes. `PipelineCursor` and `TipCursor` track
derivation state. Designed for `no_std` environments (FPVM client programs) as well as hosted
environments.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-driver = { workspace = true }
```

```rust,ignore
use base_proof_driver::{Driver, DriverPipeline, Executor};

let mut driver = Driver::new(pipeline, executor, cfg);
loop {
    driver.step().await?;
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
