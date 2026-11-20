#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]

static MAINNET_GENESIS_JSON: &str = include_str!("../res/base.json");
static SEPOLIA_GENESIS_JSON: &str = include_str!("../res/sepolia_base.json");
static DEVNET_GENESIS_JSON: &str = include_str!("../res/dev.json");
static ZERONET_GENESIS_JSON: &str = include_str!("../res/zeronet_base.json");

/// Accessors for embedded Base execution-layer genesis JSON documents.
///
/// Backed by `static`s (rather than `const` struct fields) so each ~9 MiB JSON document is emitted
/// once in this crate instead of duplicated across downstream codegen units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenesisJson;

impl GenesisJson {
    /// Returns the Base Mainnet (`8453`) execution genesis JSON.
    pub fn mainnet() -> &'static str {
        MAINNET_GENESIS_JSON
    }

    /// Returns the Base Sepolia (`84532`) execution genesis JSON.
    pub fn sepolia() -> &'static str {
        SEPOLIA_GENESIS_JSON
    }

    /// Returns the local Base devnet (`84538453`) execution genesis JSON.
    pub fn devnet() -> &'static str {
        DEVNET_GENESIS_JSON
    }

    /// Returns the Base Zeronet (`763360`) execution genesis JSON.
    pub fn zeronet() -> &'static str {
        ZERONET_GENESIS_JSON
    }

    /// Returns the embedded execution genesis JSON for a known Base chain ID, falling back to
    /// [`Self::devnet`] for custom/local chain IDs.
    pub fn for_chain_id(chain_id: u64) -> &'static str {
        match chain_id {
            8453 => MAINNET_GENESIS_JSON,
            84532 => SEPOLIA_GENESIS_JSON,
            763360 => ZERONET_GENESIS_JSON,
            _ => DEVNET_GENESIS_JSON,
        }
    }
}
