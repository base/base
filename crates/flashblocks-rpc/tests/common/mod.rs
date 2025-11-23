use std::sync::Once;

use alloy_primitives::{B256, Bytes, b256, bytes};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

pub const BLOCK_INFO_TXN: Bytes = bytes!(
    "0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000"
);
pub const BLOCK_INFO_TXN_HASH: B256 =
    b256!("0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6");

static TRACING: Once = Once::new();

pub fn init_tracing() {
    TRACING.call_once(|| {
        let mut filter = EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .from_env_lossy();

        for directive in ["reth_tasks=off", "reth_node_builder::launch::common=off"] {
            if let Ok(directive) = directive.parse() {
                filter = filter.add_directive(directive);
            }
        }

        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
    });
}
