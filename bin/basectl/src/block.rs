//! Implementation of the `basectl block <ref>` subcommand.

use alloy_provider::Network;
use alloy_rpc_types_eth::BlockNumberOrTag;
use anyhow::{Result, anyhow, bail};
use base_common_network::Base;
use basectl_cli::{
    JsonOutput, KeyValueTable, MonitoringConfig, fetch_block, format_bytes, format_gas,
    format_gwei, format_unix_timestamp,
};

/// Parses a CLI block reference into alloy's `BlockNumberOrTag`.
///
/// Adds three behaviors on top of `BlockNumberOrTag::FromStr`: bare decimal
/// numbers (alloy requires `0x` on numbers), explicit rejection of
/// 64-hex-char block-hash references, and rejection of the `pending` tag
/// (alloy's typed `Block` can't deserialize a pending block's null number
/// and hash, so accepting it here would only produce a confusing error
/// after a wasted RPC round-trip).
pub(crate) fn parse_block_ref(s: &str) -> Result<BlockNumberOrTag> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        bail!("invalid block reference: empty input");
    }
    if let Ok(number) = trimmed.parse::<u64>() {
        return Ok(BlockNumberOrTag::Number(number));
    }
    if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X"))
        && hex.len() == 64
        && hex.chars().all(|c| c.is_ascii_hexdigit())
    {
        bail!("block hash references are not supported; use a block number or tag");
    }
    let tag =
        trimmed.parse::<BlockNumberOrTag>().map_err(|e| anyhow!("invalid block reference: {e}"))?;
    if tag == BlockNumberOrTag::Pending {
        bail!(
            "the `pending` tag is not supported; use `latest`, `safe`, `finalized`, or `earliest`"
        );
    }
    Ok(tag)
}

/// Runs the `basectl block` subcommand.
pub(crate) async fn run(config: MonitoringConfig, reference: &str, json: bool) -> Result<()> {
    let block_ref = parse_block_ref(reference)?;
    let block = fetch_block(&config.rpc, block_ref).await?;
    if json {
        JsonOutput::print(&block)?;
    } else {
        print_pretty(&config.name, block_ref, &block)?;
    }
    Ok(())
}

fn print_pretty(
    network: &str,
    reference: BlockNumberOrTag,
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
    use alloy_rpc_types_eth::BlockNumberOrTag;

    use super::parse_block_ref;

    #[test]
    fn parses_decimal() {
        assert_eq!(parse_block_ref("123").unwrap(), BlockNumberOrTag::Number(123));
        assert_eq!(parse_block_ref("  42  ").unwrap(), BlockNumberOrTag::Number(42));
    }

    #[test]
    fn parses_hex() {
        assert_eq!(parse_block_ref("0x1a").unwrap(), BlockNumberOrTag::Number(26));
        assert_eq!(parse_block_ref("0X1A").unwrap(), BlockNumberOrTag::Number(26));
    }

    #[test]
    fn parses_tags() {
        assert_eq!(parse_block_ref("latest").unwrap(), BlockNumberOrTag::Latest);
        assert_eq!(parse_block_ref("safe").unwrap(), BlockNumberOrTag::Safe);
        assert_eq!(parse_block_ref("finalized").unwrap(), BlockNumberOrTag::Finalized);
        assert_eq!(parse_block_ref("earliest").unwrap(), BlockNumberOrTag::Earliest);
    }

    #[test]
    fn rejects_pending() {
        for input in ["pending", "Pending", "PENDING"] {
            let err = parse_block_ref(input).unwrap_err().to_string();
            assert!(
                err.contains("`pending` tag is not supported"),
                "expected pending rejection for {input:?}, got: {err}",
            );
        }
    }

    #[test]
    fn rejects_64_hex_char_hash() {
        let hash = format!("0x{}", "11".repeat(32));
        let err = parse_block_ref(&hash).unwrap_err().to_string();
        assert!(err.contains("hash references are not supported"));
    }

    #[test]
    fn rejects_invalid_input() {
        assert!(parse_block_ref("notatag").is_err());
        assert!(parse_block_ref("").is_err());
        assert!(parse_block_ref("   ").is_err());
    }
}
