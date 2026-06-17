//! Implementation of the `basectl block <ref>` subcommand.

use alloy_eips::BlockId;
use alloy_primitives::B256;
use alloy_provider::Network;
use alloy_rpc_types_eth::BlockNumberOrTag;
use anyhow::Result;
use base_common_network::Base;
use basectl_cli::{
    BlockRefParseError, JsonOutput, KeyValueTable, MonitoringConfig, TimestampJson, fetch_block,
    format_bytes, format_gas, format_gwei, format_unix_timestamp,
};
use serde::Serialize;

/// Parses a CLI block reference into alloy's `BlockId`.
///
/// Adds three behaviors on top of alloy's parsers: bare decimal numbers
/// (alloy requires `0x` on numbers), explicit handling of 64-hex-char block
/// hashes (returned as `BlockId::Hash`), and rejection of the `pending`
/// tag (alloy's typed `Block` can't deserialize a pending block's null
/// number and hash, so accepting it here would only produce a confusing
/// error after a wasted RPC round-trip).
pub(crate) fn parse_block_ref(s: &str) -> Result<BlockId, BlockRefParseError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(BlockRefParseError::Empty);
    }
    if let Ok(number) = trimmed.parse::<u64>() {
        return Ok(BlockId::Number(BlockNumberOrTag::Number(number)));
    }
    if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X"))
        && hex.len() == 64
        && hex.chars().all(|c| c.is_ascii_hexdigit())
    {
        let hash: B256 = trimmed
            .parse()
            .map_err(|_| BlockRefParseError::MalformedHash { raw: trimmed.to_string() })?;
        return Ok(BlockId::Hash(hash.into()));
    }
    let tag = trimmed.parse::<BlockNumberOrTag>().map_err(|e| BlockRefParseError::InvalidTag {
        raw: trimmed.to_string(),
        message: e.to_string(),
    })?;
    if tag == BlockNumberOrTag::Pending {
        return Err(BlockRefParseError::PendingUnsupported);
    }
    Ok(BlockId::Number(tag))
}

/// Runs the `basectl block` subcommand.
pub(crate) async fn run(
    config: MonitoringConfig,
    reference: &str,
    json: bool,
    raw: bool,
) -> Result<()> {
    let block_ref = parse_block_ref(reference)?;
    let block = fetch_block(&config.rpc, block_ref).await?;
    match (json, raw) {
        (true, true) => JsonOutput::print(&block)?,
        (true, false) => {
            let summary = BlockSummaryJson::from_block(&config.name, block_ref, &block);
            JsonOutput::print(&summary)?;
        }
        (false, _) => print_pretty(&config.name, block_ref, &block)?,
    }
    Ok(())
}

/// Humanized JSON shape for the `basectl block --json` output.
///
/// Mirrors the field selection of `print_pretty`, but with decoded numeric
/// values instead of the JSON-RPC wire format's hex-string quantities.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockSummaryJson {
    network: String,
    reference: String,
    number: u64,
    hash: B256,
    parent_hash: B256,
    timestamp: TimestampJson,
    transactions: usize,
    gas_used: u64,
    gas_limit: u64,
    base_fee_per_gas_wei: Option<u128>,
    size_bytes: Option<u64>,
    blob_gas_used: Option<u64>,
    excess_blob_gas: Option<u64>,
    withdrawals: Option<usize>,
}

impl BlockSummaryJson {
    fn from_block(
        network: &str,
        reference: BlockId,
        block: &<Base as Network>::BlockResponse,
    ) -> Self {
        let header = &block.header;
        Self {
            network: network.to_string(),
            reference: reference.to_string(),
            number: header.number,
            hash: header.hash,
            parent_hash: header.parent_hash,
            timestamp: TimestampJson::from_unix(header.timestamp),
            transactions: block.transactions.len(),
            gas_used: header.gas_used,
            gas_limit: header.gas_limit,
            base_fee_per_gas_wei: header.base_fee_per_gas.map(u128::from),
            size_bytes: header.size.and_then(|size| u64::try_from(size).ok()),
            blob_gas_used: header.blob_gas_used,
            excess_blob_gas: header.excess_blob_gas,
            withdrawals: block.withdrawals.as_ref().map(|w| w.len()),
        }
    }
}

fn print_pretty(
    network: &str,
    reference: BlockId,
    block: &<Base as Network>::BlockResponse,
) -> Result<()> {
    let header = &block.header;
    let mut table = KeyValueTable::new();
    table
        .row("network", network)
        .row("reference", reference.to_string())
        .row("number", header.number.to_string())
        .row("hash", format!("{:#x}", header.hash))
        .row("parent_hash", format!("{:#x}", header.parent_hash))
        .row(
            "timestamp",
            format!("{} ({})", header.timestamp, format_unix_timestamp(header.timestamp)),
        )
        .row("transactions", block.transactions.len().to_string())
        .row("gas_used", format_gas(header.gas_used))
        .row("gas_limit", format_gas(header.gas_limit));
    if let Some(base_fee) = header.base_fee_per_gas {
        table.row("base_fee_per_gas", format_gwei(u128::from(base_fee)));
    }
    if let Some(size) = header.size
        && let Ok(size_u64) = u64::try_from(size)
    {
        table.row("size", format_bytes(size_u64));
    }
    if let Some(blob_gas_used) = header.blob_gas_used {
        table.row("blob_gas_used", format_gas(blob_gas_used));
    }
    if let Some(excess_blob_gas) = header.excess_blob_gas {
        table.row("excess_blob_gas", format_gas(excess_blob_gas));
    }
    if let Some(withdrawals) = block.withdrawals.as_ref() {
        table.row("withdrawals", withdrawals.len().to_string());
    }
    table.print()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockId;
    use alloy_primitives::B256;
    use alloy_rpc_types_eth::BlockNumberOrTag;
    use basectl_cli::BlockRefParseError;

    use super::parse_block_ref;

    #[test]
    fn parses_decimal() {
        assert_eq!(parse_block_ref("123").unwrap(), BlockId::Number(BlockNumberOrTag::Number(123)),);
        assert_eq!(
            parse_block_ref("  42  ").unwrap(),
            BlockId::Number(BlockNumberOrTag::Number(42)),
        );
    }

    #[test]
    fn parses_hex() {
        assert_eq!(parse_block_ref("0x1a").unwrap(), BlockId::Number(BlockNumberOrTag::Number(26)),);
        assert_eq!(parse_block_ref("0X1A").unwrap(), BlockId::Number(BlockNumberOrTag::Number(26)),);
    }

    #[test]
    fn parses_tags() {
        assert_eq!(parse_block_ref("latest").unwrap(), BlockId::Number(BlockNumberOrTag::Latest));
        assert_eq!(parse_block_ref("safe").unwrap(), BlockId::Number(BlockNumberOrTag::Safe));
        assert_eq!(
            parse_block_ref("finalized").unwrap(),
            BlockId::Number(BlockNumberOrTag::Finalized),
        );
        assert_eq!(
            parse_block_ref("earliest").unwrap(),
            BlockId::Number(BlockNumberOrTag::Earliest),
        );
    }

    #[test]
    fn parses_block_hash() {
        let canonical = format!("0x{}", "11".repeat(32));
        let expected = canonical.parse::<B256>().unwrap();

        for input in [canonical.clone(), canonical.replace("0x", "0X"), canonical.to_uppercase()] {
            let parsed = parse_block_ref(&input).unwrap();
            let BlockId::Hash(rpc_hash) = parsed else {
                panic!("expected BlockId::Hash for {input:?}, got {parsed:?}");
            };
            assert_eq!(rpc_hash.block_hash, expected, "hash mismatch for {input:?}");
        }
    }

    #[test]
    fn rejects_pending() {
        for input in ["pending", "Pending", "PENDING"] {
            assert!(matches!(
                parse_block_ref(input).unwrap_err(),
                BlockRefParseError::PendingUnsupported
            ));
        }
    }

    #[test]
    fn rejects_invalid_input() {
        assert!(matches!(
            parse_block_ref("notatag").unwrap_err(),
            BlockRefParseError::InvalidTag { .. }
        ));
        assert!(matches!(parse_block_ref("").unwrap_err(), BlockRefParseError::Empty));
        assert!(matches!(parse_block_ref("   ").unwrap_err(), BlockRefParseError::Empty));
    }

    #[test]
    fn block_summary_json_serializes_with_camel_case_and_nested_timestamp() {
        let summary = super::BlockSummaryJson {
            network: "sepolia".to_string(),
            reference: "latest".to_string(),
            number: 42,
            hash: B256::repeat_byte(0x11),
            parent_hash: B256::repeat_byte(0x22),
            timestamp: basectl_cli::TimestampJson::from_unix(1_780_614_804),
            transactions: 7,
            gas_used: 21_000,
            gas_limit: 30_000_000,
            base_fee_per_gas_wei: Some(5_000_000),
            size_bytes: Some(500),
            blob_gas_used: Some(2),
            excess_blob_gas: None,
            withdrawals: Some(3),
        };

        let value: serde_json::Value = serde_json::to_value(&summary).unwrap();

        assert_eq!(value["network"], "sepolia");
        assert_eq!(value["reference"], "latest");
        assert_eq!(value["number"], 42);
        assert!(value["hash"].as_str().unwrap().starts_with("0x11"));
        assert!(value["parentHash"].as_str().unwrap().starts_with("0x22"));
        assert_eq!(value["timestamp"]["unix"], 1_780_614_804u64);
        assert!(value["timestamp"]["utc"].as_str().unwrap().ends_with('Z'));
        assert!(value["timestamp"]["local"].is_string());
        assert_eq!(value["transactions"], 7);
        assert_eq!(value["gasUsed"], 21_000);
        assert_eq!(value["gasLimit"], 30_000_000);
        assert_eq!(value["baseFeePerGasWei"], 5_000_000);
        assert_eq!(value["sizeBytes"], 500);
        assert_eq!(value["blobGasUsed"], 2);
        assert!(value["excessBlobGas"].is_null());
        assert_eq!(value["withdrawals"], 3);
    }
}
