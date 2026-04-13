# `base-common-flz`

`FastLZ` compression length estimation and Fjord transaction size utilities.

## Overview

Implements `FastLZ` compression size estimation and Fjord-era L1 data fee calculation. The key
function `data_gas_fjord` computes how much L1 data gas a transaction consumes after `FastLZ`
compression, using the Fjord cost model (16 gas/byte compressed). Also provides
`tx_estimated_size_fjord` for raw size estimates and constants for the fee formula coefficients.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-common-flz = { workspace = true }
```

```rust,ignore
use base_common_flz::{data_gas_fjord, flz_compress_len};

let compressed_len = flz_compress_len(&tx_data);
let l1_data_gas = data_gas_fjord(&tx_data);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
