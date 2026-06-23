//! Implementation of the `basectl txpool` command group.

use std::io::{self, Write};

use alloy_primitives::{Address, TxHash};
use anyhow::Result;
use basectl_cli::{
    JsonOutput, KeyValueTable, MonitoringConfig, TxpoolClient, TxpoolCounts, TxpoolReport,
    TxpoolScope, TxpoolSenderSummary, TxpoolTransactionRow, format_gas, format_gwei,
};
use serde::Serialize;
use tracing::{debug, info, warn};
use url::Url;

use crate::{
    cli::{TxpoolClearArgs, TxpoolCommands, TxpoolReadArgs},
    confirm::confirm_or_abort,
};

/// Runs the `basectl txpool` command group.
pub(crate) async fn run(config: MonitoringConfig, command: TxpoolCommands) -> Result<()> {
    match command {
        TxpoolCommands::Pending(args) => run_read(config, TxpoolScope::Pending, args).await,
        TxpoolCommands::Queued(args) => run_read(config, TxpoolScope::Queued, args).await,
        TxpoolCommands::All(args) => run_read(config, TxpoolScope::All, args).await,
        TxpoolCommands::Clear(args) => run_clear(config, args).await,
    }
}

async fn run_read(
    config: MonitoringConfig,
    scope: TxpoolScope,
    args: TxpoolReadArgs,
) -> Result<()> {
    let TxpoolReadArgs { sender, el_rpc: el_rpc_override, json, raw } = args;
    let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
    info!(
        network = %config.name,
        rpc = %el_rpc,
        scope = %scope.as_str(),
        sender = ?sender,
        json,
        raw,
        "fetching txpool content"
    );

    match (json, raw, sender) {
        (true, true, Some(sender)) => {
            let mut content = TxpoolClient::fetch_txpool_content_from(&el_rpc, sender)
                .await
                .inspect_err(|error| {
                    warn!(
                        error = %error,
                        network = %config.name,
                        rpc = %el_rpc,
                        scope = %scope.as_str(),
                        sender = %sender,
                        raw = true,
                        "txpool sender-filtered raw read failed"
                    );
                })?;
            scope.filter_content_from(&mut content);
            JsonOutput::print(&content)?;
        }
        (true, true, None) => {
            let mut content =
                TxpoolClient::fetch_txpool_content(&el_rpc).await.inspect_err(|error| {
                    warn!(
                        error = %error,
                        network = %config.name,
                        rpc = %el_rpc,
                        scope = %scope.as_str(),
                        raw = true,
                        "txpool raw read failed"
                    );
                })?;
            scope.filter_content(&mut content);
            JsonOutput::print(&content)?;
        }
        (true, false, sender) => {
            let report = TxpoolClient::fetch_txpool_report(&el_rpc, scope, sender)
                .await
                .inspect_err(|error| {
                    warn!(
                        error = %error,
                        network = %config.name,
                        rpc = %el_rpc,
                        scope = %scope.as_str(),
                        sender = ?sender,
                        raw = false,
                        "txpool humanized JSON read failed"
                    );
                })?;
            JsonOutput::print(&TxpoolReadJson::from_report(&config.name, &el_rpc, &report))?;
            debug!(
                network = %config.name,
                rpc = %el_rpc,
                scope = %scope.as_str(),
                sender = ?sender,
                pending = report.counts.pending,
                queued = report.counts.queued,
                total = report.counts.total,
                "txpool humanized JSON read completed"
            );
        }
        (false, _, sender) => {
            let report = TxpoolClient::fetch_txpool_report(&el_rpc, scope, sender)
                .await
                .inspect_err(|error| {
                    warn!(
                        error = %error,
                        network = %config.name,
                        rpc = %el_rpc,
                        scope = %scope.as_str(),
                        sender = ?sender,
                        raw = false,
                        "txpool pretty read failed"
                    );
                })?;
            print_read_pretty(&config.name, &el_rpc, &report)?;
            debug!(
                network = %config.name,
                rpc = %el_rpc,
                scope = %scope.as_str(),
                sender = ?sender,
                pending = report.counts.pending,
                queued = report.counts.queued,
                total = report.counts.total,
                "txpool pretty read completed"
            );
        }
    }

    Ok(())
}

async fn run_clear(config: MonitoringConfig, args: TxpoolClearArgs) -> Result<()> {
    let TxpoolClearArgs { sender, el_rpc: el_rpc_override, yes, json } = args;
    let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
    info!(
        network = %config.name,
        rpc = %el_rpc,
        sender = ?sender,
        json,
        yes,
        "running txpool clear command"
    );

    match sender {
        Some(sender) => {
            let prompt = format!("Drop txpool transactions from {sender} through {el_rpc}? [y/N] ");
            if !confirm_or_abort(&prompt, yes)? {
                debug!(
                    network = %config.name,
                    rpc = %el_rpc,
                    sender = %sender,
                    "txpool sender drop confirmation declined"
                );
                return Ok(());
            }
            let hashes = TxpoolClient::drop_sender_transactions(&el_rpc, sender)
                .await
                .inspect_err(|error| {
                    warn!(
                        error = %error,
                        network = %config.name,
                        rpc = %el_rpc,
                        sender = %sender,
                        "txpool sender drop failed"
                    );
                })?;
            let action = TxpoolClearJson::drop_sender(&config.name, &el_rpc, sender, hashes);
            print_clear_action(&action, json)?;
            info!(
                network = %config.name,
                rpc = %el_rpc,
                sender = %sender,
                removed = action.removed(),
                "txpool sender drop completed"
            );
        }
        None => {
            let prompt = format!("Clear all txpool transactions through {el_rpc}? [y/N] ");
            if !confirm_or_abort(&prompt, yes)? {
                debug!(
                    network = %config.name,
                    rpc = %el_rpc,
                    "txpool clear confirmation declined"
                );
                return Ok(());
            }
            let removed = TxpoolClient::clear_txpool(&el_rpc).await.inspect_err(|error| {
                warn!(
                    error = %error,
                    network = %config.name,
                    rpc = %el_rpc,
                    "txpool clear failed"
                );
            })?;
            let action = TxpoolClearJson::clear(&config.name, &el_rpc, removed);
            print_clear_action(&action, json)?;
            info!(
                network = %config.name,
                rpc = %el_rpc,
                removed,
                "txpool clear completed"
            );
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TxpoolReadJson {
    network: String,
    rpc: String,
    scope: TxpoolScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    sender: Option<Address>,
    counts: TxpoolCounts,
    senders: Vec<TxpoolSenderSummary>,
    transactions: Vec<TxpoolTransactionRow>,
}

impl TxpoolReadJson {
    fn from_report(network: &str, rpc: &Url, report: &TxpoolReport) -> Self {
        Self {
            network: network.to_string(),
            rpc: rpc.to_string(),
            scope: report.scope,
            sender: report.sender,
            counts: report.counts,
            senders: report.senders.clone(),
            transactions: report.transactions.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "action")]
enum TxpoolClearJson {
    #[serde(rename = "clearTxpool")]
    Clear { network: String, rpc: String, removed: u64 },
    #[serde(rename = "dropSenderTransactions")]
    DropSender {
        network: String,
        rpc: String,
        sender: Address,
        removed: u64,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        hashes: Vec<TxHash>,
    },
}

impl TxpoolClearJson {
    fn clear(network: &str, rpc: &Url, removed: u64) -> Self {
        Self::Clear { network: network.to_string(), rpc: rpc.to_string(), removed }
    }

    fn drop_sender(network: &str, rpc: &Url, sender: Address, hashes: Vec<TxHash>) -> Self {
        let removed = u64::try_from(hashes.len()).unwrap_or(u64::MAX);
        Self::DropSender {
            network: network.to_string(),
            rpc: rpc.to_string(),
            sender,
            removed,
            hashes,
        }
    }

    const fn removed(&self) -> u64 {
        match self {
            Self::Clear { removed, .. } | Self::DropSender { removed, .. } => *removed,
        }
    }
}

fn print_read_pretty(network: &str, rpc: &Url, report: &TxpoolReport) -> Result<()> {
    let mut stdout = io::stdout().lock();
    print_read_pretty_to(&mut stdout, network, rpc, report)?;
    Ok(())
}

fn print_read_pretty_to<W: Write>(
    writer: &mut W,
    network: &str,
    rpc: &Url,
    report: &TxpoolReport,
) -> Result<()> {
    let mut table = KeyValueTable::new();
    table.row("network", network).row("rpc", rpc.to_string()).row("scope", report.scope.as_str());
    match report.scope {
        TxpoolScope::Pending => {
            table.row("pending", report.counts.pending.to_string());
        }
        TxpoolScope::Queued => {
            table.row("queued", report.counts.queued.to_string());
        }
        TxpoolScope::All => {
            table
                .row("pending", report.counts.pending.to_string())
                .row("queued", report.counts.queued.to_string())
                .row("total", report.counts.total.to_string());
        }
    }
    if let Some(sender) = report.sender {
        table.row("sender", sender.to_string());
    }
    table.render(writer)?;

    if report.senders.is_empty() || report.counts.total == 0 {
        writeln!(writer, "no transactions")?;
        return Ok(());
    }

    writeln!(writer, "senders")?;
    for summary in &report.senders {
        writeln!(
            writer,
            "  {} pending={} queued={} total={} nonces={}",
            summary.sender,
            summary.pending,
            summary.queued,
            summary.total,
            format_nonce_range(summary)
        )?;
    }

    writeln!(writer, "transactions")?;
    for transaction in &report.transactions {
        writeln!(
            writer,
            "  {pool:<7} sender={sender} nonce={nonce} hash={hash} to={to} value={value_wei} wei gas={gas} fee={fee} input={input}B",
            pool = transaction.pool.as_str(),
            sender = transaction.sender,
            nonce = transaction.nonce,
            hash = transaction.hash,
            to = format_destination(transaction.to),
            value_wei = transaction.value_wei,
            gas = format_gas(transaction.gas_limit),
            fee = format_transaction_fee(transaction),
            input = transaction.input_bytes,
        )?;
    }

    Ok(())
}

fn format_nonce_range(summary: &TxpoolSenderSummary) -> String {
    match (summary.lowest_nonce, summary.highest_nonce) {
        (Some(low), Some(high)) if low == high => low.to_string(),
        (Some(low), Some(high)) => format!("{low}..{high}"),
        _ => "none".to_string(),
    }
}

fn format_destination(to: Option<Address>) -> String {
    to.map_or_else(|| "create".to_string(), |to| to.to_string())
}

fn format_transaction_fee(transaction: &TxpoolTransactionRow) -> String {
    let priority_fee =
        transaction.max_priority_fee_per_gas_wei.map_or_else(|| "n/a".to_string(), format_gwei);
    format!("max={} priority={priority_fee}", format_gwei(transaction.max_fee_per_gas_wei))
}

fn print_clear_action(action: &TxpoolClearJson, json: bool) -> Result<()> {
    if json {
        JsonOutput::print(action)?;
    } else {
        print_clear_action_pretty(action)?;
    }
    Ok(())
}

fn print_clear_action_pretty(action: &TxpoolClearJson) -> Result<()> {
    let mut stdout = io::stdout().lock();
    match action {
        TxpoolClearJson::Clear { removed, .. } => {
            writeln!(stdout, "OK cleared {removed} txpool transaction(s)")?;
        }
        TxpoolClearJson::DropSender { sender, removed, .. } => {
            writeln!(stdout, "OK dropped {removed} txpool transaction(s) from {sender}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, address};
    use basectl_cli::{
        TxpoolCounts, TxpoolReport, TxpoolScope, TxpoolSenderSummary, TxpoolTransactionPool,
        TxpoolTransactionRow,
    };
    use serde_json::json;
    use url::Url;

    use super::{
        TxpoolClearJson, TxpoolReadJson, format_destination, format_nonce_range,
        format_transaction_fee, print_read_pretty_to,
    };

    fn sample_report() -> TxpoolReport {
        let sender = address!("1111111111111111111111111111111111111111");
        TxpoolReport {
            scope: TxpoolScope::All,
            sender: Some(sender),
            counts: TxpoolCounts::new(1, 1),
            senders: vec![TxpoolSenderSummary {
                sender,
                pending: 1,
                queued: 1,
                total: 2,
                lowest_nonce: Some(1),
                highest_nonce: Some(2),
            }],
            transactions: vec![TxpoolTransactionRow {
                pool: TxpoolTransactionPool::Pending,
                sender,
                nonce: 1,
                nonce_key: "1".to_string(),
                hash: B256::repeat_byte(0x11),
                tx_type: 2,
                to: Some(address!("2222222222222222222222222222222222222222")),
                value_wei: "123".to_string(),
                gas_limit: 21_000,
                gas_price_wei: Some(1_000_000_000),
                max_fee_per_gas_wei: 1_000_000_000,
                max_priority_fee_per_gas_wei: Some(100_000_000),
                input_bytes: 2,
            }],
        }
    }

    #[test]
    fn humanized_read_json_shape() {
        let rpc = Url::parse("http://127.0.0.1:8545").unwrap();
        let value =
            serde_json::to_value(TxpoolReadJson::from_report("devnet", &rpc, &sample_report()))
                .unwrap();

        assert_eq!(value["network"], "devnet");
        assert_eq!(value["rpc"], "http://127.0.0.1:8545/");
        assert_eq!(value["scope"], "all");
        assert_eq!(value["counts"]["total"], 2);
        assert_eq!(value["senders"][0]["pending"], 1);
        assert_eq!(value["transactions"][0]["pool"], "pending");
        assert_eq!(value["transactions"][0]["valueWei"], "123");
    }

    #[test]
    fn clear_action_json_shapes() {
        let rpc = Url::parse("http://127.0.0.1:8545").unwrap();
        let clear = serde_json::to_value(TxpoolClearJson::clear("devnet", &rpc, 4)).unwrap();
        assert_eq!(
            clear,
            json!({
                "network": "devnet",
                "rpc": "http://127.0.0.1:8545/",
                "action": "clearTxpool",
                "removed": 4
            })
        );

        let sender = address!("1111111111111111111111111111111111111111");
        let hash = B256::repeat_byte(0x22);
        let drop_sender =
            serde_json::to_value(TxpoolClearJson::drop_sender("devnet", &rpc, sender, vec![hash]))
                .unwrap();
        assert_eq!(
            drop_sender,
            json!({
                "network": "devnet",
                "rpc": "http://127.0.0.1:8545/",
                "action": "dropSenderTransactions",
                "sender": sender.to_string(),
                "removed": 1,
                "hashes": [hash.to_string()]
            })
        );
    }

    #[test]
    fn pretty_output_smoke() {
        let rpc = Url::parse("http://127.0.0.1:8545").unwrap();
        let report = sample_report();
        let mut output = Vec::new();

        print_read_pretty_to(&mut output, "devnet", &rpc, &report).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("network  devnet"));
        assert!(rendered.contains("scope    all"));
        assert!(rendered.contains("pending  1"));
        assert!(rendered.contains("queued   1"));
        assert!(rendered.contains("total    2"));
        assert!(rendered.contains("senders"));
        assert!(rendered.contains("pending=1 queued=1 total=2 nonces=1..2"));
        assert!(rendered.contains("transactions"));
        assert!(rendered.contains("pending sender=0x1111111111111111111111111111111111111111"));
        assert!(rendered.contains("nonce=1"));
        assert!(
            rendered.contains(
                "hash=0x1111111111111111111111111111111111111111111111111111111111111111"
            )
        );
        assert!(rendered.contains("to=0x2222222222222222222222222222222222222222"));
        assert!(rendered.contains("value=123 wei"));
        assert!(rendered.contains("gas=21K"));
        assert!(rendered.contains("fee=max=1.00 gwei priority=0.1000 gwei"));
        assert!(rendered.contains("input=2B"));
    }

    #[test]
    fn pretty_output_shows_only_scoped_counts() {
        let rpc = Url::parse("http://127.0.0.1:8545").unwrap();
        let mut report = sample_report();

        report.scope = TxpoolScope::Pending;
        report.counts = TxpoolCounts::new(1, 0);
        let mut pending_output = Vec::new();
        print_read_pretty_to(&mut pending_output, "devnet", &rpc, &report).unwrap();
        let pending = String::from_utf8(pending_output).unwrap();
        assert!(pending.contains("pending  1"));
        assert!(!pending.contains("\nqueued"));
        assert!(!pending.contains("\ntotal"));

        report.scope = TxpoolScope::Queued;
        report.counts = TxpoolCounts::new(0, 1);
        let mut queued_output = Vec::new();
        print_read_pretty_to(&mut queued_output, "devnet", &rpc, &report).unwrap();
        let queued = String::from_utf8(queued_output).unwrap();
        assert!(queued.contains("queued   1"));
        assert!(!queued.contains("\npending"));
        assert!(!queued.contains("\ntotal"));
    }

    #[test]
    fn formats_nonce_ranges() {
        let sender = address!("1111111111111111111111111111111111111111");
        let mut summary = TxpoolSenderSummary::empty(sender);
        assert_eq!(format_nonce_range(&summary), "none");

        summary.lowest_nonce = Some(7);
        summary.highest_nonce = Some(7);
        assert_eq!(format_nonce_range(&summary), "7");

        summary.highest_nonce = Some(9);
        assert_eq!(format_nonce_range(&summary), "7..9");
    }

    #[test]
    fn formats_transaction_fields_for_pretty_output() {
        let sender = address!("1111111111111111111111111111111111111111");
        let transaction = TxpoolTransactionRow {
            pool: TxpoolTransactionPool::Queued,
            sender,
            nonce: 1,
            nonce_key: "1".to_string(),
            hash: B256::repeat_byte(0x11),
            tx_type: 2,
            to: None,
            value_wei: "1".to_string(),
            gas_limit: 21_000,
            gas_price_wei: None,
            max_fee_per_gas_wei: 2_000_000_000,
            max_priority_fee_per_gas_wei: None,
            input_bytes: 0,
        };

        assert_eq!(format_destination(transaction.to), "create");
        assert_eq!(format_transaction_fee(&transaction), "max=2.00 gwei priority=n/a");
    }
}
