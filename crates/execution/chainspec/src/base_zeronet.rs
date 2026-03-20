//! Chain specification for the Base zeronet network.

use alloc::sync::Arc;

use reth_primitives_traits::sync::LazyLock;

use crate::OpChainSpec;

/// The Base zeronet spec
pub static BASE_ZERONET: LazyLock<Arc<OpChainSpec>> = LazyLock::new(|| {
    let genesis = serde_json::from_str(include_str!("../res/genesis/base-zeronet.json"))
        .expect("Can't deserialize Base zeronet genesis json");
    OpChainSpec::from_genesis(genesis).into()
});
