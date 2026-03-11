# `base-execution-payload-builder`

Payload builder for Base.

## Overview

Implements Base payload building and validation for the Base execution node. The
`OpPayloadBuilder` assembles new execution payloads from transaction pool contents and
`OpPayloadAttributes` received from the consensus layer. `OpExecutionPayloadValidator` verifies
built payloads against consensus rules. Also provides data availability configuration via
`OpDAConfig` for fee calculation.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-payload-builder = { workspace = true }
```

```rust,ignore
use base_execution_payload_builder::OpPayloadBuilder;

let builder = OpPayloadBuilder::new(evm_config, payload_validator);
let payload = builder.build_payload(attrs, best_payload)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
