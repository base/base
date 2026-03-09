# `base-alloy-provider`

Base chain providers for the engine API, adopted from L1, and Base-unique engine API extensions.

## Overview

Extends the alloy provider with OP Stack engine API functionality. Provides the `OpEngineApi`
trait with OP-specific engine methods for block building and forkchoice management, used
internally by execution node implementations to communicate with the consensus layer via the
Engine API.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-alloy-provider = { workspace = true }
```

```rust,ignore
use base_alloy_provider::OpEngineApi;

let payload_id = client.fork_choice_updated_v3(state, attrs).await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
