//! Implementation of the `basectl block <ref>` subcommand.

use alloy_eips::BlockId;
use alloy_primitives::B256;
use alloy_provider::Network;
use alloy_rpc_types_eth::BlockNumberOrTag;
use anyhow::{Result, anyhow, bail};
use base_common_network::Base;
use basectl_cli::{
    JsonOutput, KeyValueTable, MonitoringConfig, fetch_block, format_bytes, format_gas,
    format_gwei, format_unix_timestamp,
};

/// Parses a CLI block reference into alloy's `BlockId`.
///
/// Adds three behaviors on top of alloy's parsers: bare decimal numbers
/// (alloy requires `0x` on numbers), explicit handling of 64-hex-char block
/// hashes (returned as `BlockId::Hash`), and rejection of the `pending`
/// tag (alloy's typed `Block` can't deserialize a pending block's null
/// number and hash, so accepting it here would only produce a confusing
/// error after a wasted RPC round-trip).
pub(crate) fn parse_block_ref(s: &str) -> Result<BlockId> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        bail!("invalid block reference: empty input");
    }
    if let Ok(number) = trimmed.parse::<u64>() {
        return Ok(BlockId::Number(BlockNumberOrTag::Number(number)));
    }
    if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X"))
        && hex.len() == 64
        && hex.chars().all(|c| c.is_ascii_hexdigit())
    {
        let hash: B256 =
            trimmed.parse().map_err(|_| anyhow!("invalid block reference: malformed hash"))?;
        return Ok(BlockId::Hash(hash.into()));
    }
    let tag =
        trimmed.parse::<BlockNumberOrTag>().map_err(|e| anyhow!("invalid block reference: {e}"))?;
    if tag == BlockNumberOrTag::Pending {
        bail!(
            "the `pending` tag is not supported; use `latest`, `safe`, `finalized`, or `earliest`"
        );
    }
    Ok(BlockId::Number(tag))
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
            let err = parse_block_ref(input).unwrap_err().to_string();
            assert!(
                err.contains("`pending` tag is not supported"),
                "expected pending rejection for {input:?}, got: {err}",
            );
        }
    }

    #[test]
    fn rejects_invalid_input() {
        assert!(parse_block_ref("notatag").is_err());
        assert!(parse_block_ref("").is_err());
        assert!(parse_block_ref("   ").is_err());
    }
}
