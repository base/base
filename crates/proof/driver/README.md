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
use base_proof_driver::Driver;

// `cursor` is the shared `Arc<RwLock<PipelineCursor>>` the pipeline was built from.
let mut driver = Driver::new(cursor, executor, pipeline);

// Derive and execute until the safe head reaches `target` (or the data source is exhausted),
// calling the closure with each derived block and its output root.
let (safe_head, output_root) =
    driver.advance_to_target(&rollup_config, Some(target), |_block, _output_root| {}).await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
