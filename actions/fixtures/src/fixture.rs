use std::{fmt, str::FromStr};

use alloy_consensus::{Header, Receipt};
use alloy_eips::eip4844::Blob;
use alloy_primitives::{B256, Bytes};
use base_common_genesis::SystemConfig;
use base_protocol::L2BlockInfo;
use serde::{Deserialize, Serialize};

/// The current fixture schema version.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// The type of behavior a fixture is intended to test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FixtureKind {
    /// A derivation fixture backed by captured L1 data and expected L2 outputs.
    Derivation,
    /// An execution fixture backed by captured L2 block execution data.
    Execution,
}

impl fmt::Display for FixtureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Derivation => f.write_str("derivation"),
            Self::Execution => f.write_str("execution"),
        }
    }
}

impl FromStr for FixtureKind {
    type Err = FixtureKindParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "derivation" => Ok(Self::Derivation),
            "execution" => Ok(Self::Execution),
            _ => Err(FixtureKindParseError { value: value.to_owned() }),
        }
    }
}

/// Error returned when parsing a [`FixtureKind`] from text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown fixture kind: {value}")]
pub struct FixtureKindParseError {
    /// The unrecognized fixture kind.
    pub value: String,
}

/// Block identity used by fixture manifests and expected outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockId {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
}

/// Provenance, schema, and range anchors for a checked-in fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FixtureManifest {
    /// Fixture schema version.
    pub schema_version: u32,
    /// Stable fixture name within its network directory.
    pub name: String,
    /// Source network name, such as `base-mainnet` or `base-sepolia`.
    pub network: String,
    /// Fixture behavior category.
    pub kind: FixtureKind,
    /// Human-readable source description, usually `rpc-capture`.
    pub source: String,
    /// RFC 3339 capture timestamp, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    /// First captured L1 block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l1_start: Option<BlockId>,
    /// Last captured L1 block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l1_end: Option<BlockId>,
    /// First captured L2 block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l2_start: Option<BlockId>,
    /// Last captured L2 block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l2_end: Option<BlockId>,
}

impl FixtureManifest {
    /// Create a manifest with the current schema version.
    pub fn new(name: impl Into<String>, network: impl Into<String>, kind: FixtureKind) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            name: name.into(),
            network: network.into(),
            kind,
            source: "manual".to_owned(),
            captured_at: None,
            l1_start: None,
            l1_end: None,
            l2_start: None,
            l2_end: None,
        }
    }
}

/// Captured EIP-4844 blob sidecar data keyed by versioned hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixtureBlob {
    /// Versioned blob hash referenced by a blob transaction.
    pub versioned_hash: B256,
    /// Blob data.
    pub data: Blob,
}

/// Captured L1 block data required by derivation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixtureL1Block {
    /// Full consensus header.
    pub header: Header,
    /// EIP-2718 encoded L1 transaction bytes, in block order.
    pub transactions: Vec<Bytes>,
    /// Consensus receipts, in transaction order.
    pub receipts: Vec<Receipt>,
    /// Blob sidecars needed by the block's blob transactions.
    #[serde(default)]
    pub blobs: Vec<FixtureBlob>,
}

impl FixtureL1Block {
    /// Return this block's identity.
    pub fn id(&self) -> BlockId {
        BlockId { number: self.header.number, hash: self.header.hash_slow() }
    }
}

/// Captured L2 block data used for expected outcomes and execution fixtures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixtureL2Block {
    /// Full consensus header.
    pub header: Header,
    /// EIP-2718 encoded L2 transaction bytes, in block order.
    pub transactions: Vec<Bytes>,
    /// Consensus receipts, in transaction order.
    pub receipts: Vec<Receipt>,
    /// L1 origin block for this L2 block, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l1_origin: Option<BlockId>,
}

impl FixtureL2Block {
    /// Return this block's identity.
    pub fn id(&self) -> BlockId {
        BlockId { number: self.header.number, hash: self.header.hash_slow() }
    }
}

/// Derivation replay state anchored immediately before the expected L2 range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivationFixture {
    /// Safe L2 head to use as the derivation cursor.
    pub safe_head: L2BlockInfo,
    /// System configuration active at the safe head.
    pub system_config: SystemConfig,
    /// Optional L2 blocks before the expected range, used for reset and span-batch validation.
    #[serde(default)]
    pub l2_history: Vec<FixtureL2Block>,
}

/// Expected payload identity for a derived L2 block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectedPayload {
    /// L2 block number.
    pub number: u64,
    /// Expected L2 block hash, if the fixture records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<B256>,
    /// Expected state root, if the fixture records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_root: Option<B256>,
}

/// Expected state root for a block number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateRoot {
    /// L2 block number.
    pub number: u64,
    /// Expected state root.
    pub state_root: B256,
}

/// Expected fixture outcome after replaying or deriving the captured data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectedOutcome {
    /// Expected final safe head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_head: Option<BlockId>,
    /// Expected derived payloads by block number.
    #[serde(default)]
    pub derived_payloads: Vec<ExpectedPayload>,
    /// Expected state roots by block number.
    #[serde(default)]
    pub state_roots: Vec<StateRoot>,
}

/// Top-level typed fixture object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionFixture {
    /// Fixture manifest.
    pub manifest: FixtureManifest,
    /// Captured L1 block data.
    #[serde(default)]
    pub l1_blocks: Vec<FixtureL1Block>,
    /// Captured L2 block data.
    #[serde(default)]
    pub l2_blocks: Vec<FixtureL2Block>,
    /// Expected result metadata.
    #[serde(default)]
    pub expected: ExpectedOutcome,
    /// Derivation replay anchor and history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derivation: Option<DerivationFixture>,
}

impl ActionFixture {
    /// Construct a fixture from its typed parts.
    pub const fn new(
        manifest: FixtureManifest,
        l1_blocks: Vec<FixtureL1Block>,
        l2_blocks: Vec<FixtureL2Block>,
        expected: ExpectedOutcome,
    ) -> Self {
        Self { manifest, l1_blocks, l2_blocks, expected, derivation: None }
    }

    /// Attach derivation replay state to the fixture.
    pub fn with_derivation(mut self, derivation: DerivationFixture) -> Self {
        self.derivation = Some(derivation);
        self
    }
}

/// File names used by the fixture directory format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixturePaths;

impl FixturePaths {
    /// Manifest file name.
    pub const MANIFEST: &'static str = "manifest.toml";
    /// L1 blocks JSON file name.
    pub const L1: &'static str = "l1.json";
    /// L2 blocks JSON file name.
    pub const L2: &'static str = "l2.json";
    /// Expected outcome JSON file name.
    pub const EXPECTED: &'static str = "expected.json";
    /// Derivation replay JSON file name.
    pub const DERIVATION: &'static str = "derivation.json";
    /// Single-file fixture JSON file name.
    pub const FIXTURE: &'static str = "fixture.json";
}

#[cfg(test)]
mod tests {
    use super::{FixtureKind, FixtureManifest};

    #[test]
    fn parses_fixture_kind() {
        assert_eq!("derivation".parse::<FixtureKind>().unwrap(), FixtureKind::Derivation);
        assert!("unknown".parse::<FixtureKind>().is_err());
    }

    #[test]
    fn manifest_defaults_to_current_schema() {
        let manifest = FixtureManifest::new("window", "base-mainnet", FixtureKind::Derivation);
        assert_eq!(manifest.schema_version, 1);
    }
}
