use std::{borrow::Cow, collections::HashSet, io::Write};

use brotli::DecompressorWriter;
use serde_json::{self, Value};
use tracing::{debug, info, trace, warn};

/// Determines how multiple filter conditions are combined.
#[derive(Debug, Clone, Copy, Default)]
pub enum MatchMode {
    /// OR logic - match if any condition is true.
    #[default]
    Any,
    /// AND logic - match only if all conditions are true.
    All,
}

/// Specifies which criteria to use when filtering incoming payloads.
#[derive(Debug, Clone)]
pub enum FilterType {
    /// Filter by a set of contract or account addresses.
    Addresses(HashSet<String>),
    /// Filter by a set of event log topics.
    Topics(HashSet<String>),
    /// Filter by both addresses and topics using the given [`MatchMode`].
    Combined {
        /// The set of addresses to match against.
        addresses: HashSet<String>,
        /// The set of topics to match against.
        topics: HashSet<String>,
        /// How to combine address and topic match results.
        match_mode: MatchMode,
    },
    /// No filter applied; all payloads match.
    None,
}

/// Maximum number of addresses or topics permitted in a single filter.
///
/// Filter matching is `O(transactions × entries)` per upstream message and runs
/// synchronously on a runtime worker, so an unbounded entry count lets one
/// connection monopolize a worker and stall the fan-out for every other client
/// (CWE-400). This caps the per-connection matching cost.
pub const MAX_FILTER_ENTRIES: usize = 1000;

/// Error returned when untrusted filter input cannot be turned into a
/// [`FilterType`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilterError {
    /// The filter declares more entries than [`MAX_FILTER_ENTRIES`].
    #[error("filter specifies {count} {kind}, exceeding the maximum of {max}")]
    TooManyEntries {
        /// Which list overflowed (`"addresses"` or `"topics"`).
        kind: &'static str,
        /// The number of entries supplied.
        count: usize,
        /// The permitted maximum ([`MAX_FILTER_ENTRIES`]).
        max: usize,
    },
}

impl FilterType {
    /// Rejects a list that exceeds [`MAX_FILTER_ENTRIES`].
    const fn check_len(kind: &'static str, count: usize) -> Result<(), FilterError> {
        if count > MAX_FILTER_ENTRIES {
            return Err(FilterError::TooManyEntries { kind, count, max: MAX_FILTER_ENTRIES });
        }
        Ok(())
    }

    /// Creates an address-only filter, or [`FilterType::None`] if the list is empty.
    ///
    /// Returns [`FilterError::TooManyEntries`] if more than [`MAX_FILTER_ENTRIES`]
    /// addresses are supplied.
    pub fn new_addresses(addresses: Vec<String>) -> Result<Self, FilterError> {
        Self::check_len("addresses", addresses.len())?;
        if addresses.is_empty() {
            Ok(Self::None)
        } else {
            let normalized: HashSet<String> =
                addresses.into_iter().map(|addr| addr.to_lowercase()).collect();
            Ok(Self::Addresses(normalized))
        }
    }

    /// Creates a topic-only filter, or [`FilterType::None`] if the list is empty.
    ///
    /// Returns [`FilterError::TooManyEntries`] if more than [`MAX_FILTER_ENTRIES`]
    /// topics are supplied.
    pub fn new_topics(topics: Vec<String>) -> Result<Self, FilterError> {
        Self::check_len("topics", topics.len())?;
        if topics.is_empty() {
            Ok(Self::None)
        } else {
            let normalized: HashSet<String> =
                topics.into_iter().map(|topic| topic.to_lowercase()).collect();
            Ok(Self::Topics(normalized))
        }
    }

    /// Creates a combined address-and-topic filter with the specified [`MatchMode`].
    ///
    /// Returns [`FilterError::TooManyEntries`] if either list exceeds
    /// [`MAX_FILTER_ENTRIES`].
    pub fn new_combined_with_mode(
        addresses: Vec<String>,
        topics: Vec<String>,
        match_mode: MatchMode,
    ) -> Result<Self, FilterError> {
        Self::check_len("addresses", addresses.len())?;
        Self::check_len("topics", topics.len())?;
        if addresses.is_empty() && topics.is_empty() {
            Ok(Self::None)
        } else if addresses.is_empty() {
            Self::new_topics(topics)
        } else if topics.is_empty() {
            Self::new_addresses(addresses)
        } else {
            let normalized_addresses: HashSet<String> =
                addresses.into_iter().map(|addr| addr.to_lowercase()).collect();
            let normalized_topics: HashSet<String> =
                topics.into_iter().map(|topic| topic.to_lowercase()).collect();
            Ok(Self::Combined {
                addresses: normalized_addresses,
                topics: normalized_topics,
                match_mode,
            })
        }
    }

    /// Returns `true` if the payload matches this filter's criteria.
    pub fn matches(&self, payload: &[u8], enable_compression: bool) -> bool {
        if let Self::None = self {
            return true;
        }

        let uncompressed_data = if enable_compression {
            let mut uncompressed_bytes = Vec::new();
            {
                let mut decoder = DecompressorWriter::new(&mut uncompressed_bytes, 4096);
                match decoder.write_all(payload) {
                    Ok(_) => (),
                    Err(e) => {
                        info!(error = %e, "error while decoding payload");
                        return false;
                    }
                }
            }
            Cow::Owned(uncompressed_bytes)
        } else {
            Cow::Borrowed(payload)
        };

        let json_result: Result<Value, _> = serde_json::from_slice(uncompressed_data.as_ref());
        match json_result {
            Ok(json) => {
                let result = self.json_matches(&json);
                trace!(result = result, filter_type = ?self, "Filter result");
                result
            }
            Err(e) => {
                warn!(
                    message = "Failed to parse JSON payload for filtering",
                    error = e.to_string()
                );
                false
            }
        }
    }

    fn json_matches(&self, json: &Value) -> bool {
        match self {
            Self::Addresses(addresses) => self.contains_any_address(json, addresses),
            Self::Topics(topics) => self.contains_any_topic(json, topics),
            Self::Combined { addresses, topics, match_mode } => {
                let address_matches = self.contains_any_address(json, addresses);
                let topic_matches = self.contains_any_topic(json, topics);

                match match_mode {
                    MatchMode::Any => {
                        // OR logic: either address OR topic must match
                        address_matches || topic_matches
                    }
                    MatchMode::All => {
                        // AND logic: both address AND topic must match
                        address_matches && topic_matches
                    }
                }
            }
            Self::None => true,
        }
    }

    fn contains_any_address(&self, json: &Value, addresses: &HashSet<String>) -> bool {
        // Optimized search: early return on first match

        // Check new_account_balances first (most direct lookup)
        if let Some(found) = json
            .get("metadata")
            .and_then(|m| m.get("new_account_balances"))
            .and_then(|b| b.as_object())
        {
            for account in found.keys() {
                if addresses.contains(&account.to_lowercase()) {
                    debug!(account = %account, "Found address in new_account_balances");
                    return true;
                }
            }
        }

        // Check logs in receipts (most common case for filtering)
        if let Some(receipts) =
            json.get("metadata").and_then(|m| m.get("receipts")).and_then(|r| r.as_object())
        {
            for receipt_value in receipts.values() {
                if let Some(receipt_obj) = receipt_value.as_object() {
                    for receipt_data in receipt_obj.values() {
                        if let Some(logs) =
                            receipt_data.get("logs").and_then(|logs| logs.as_array())
                        {
                            for log in logs {
                                if let Some(addr_str) =
                                    log.get("address").and_then(|addr| addr.as_str())
                                    && addresses.contains(&addr_str.to_lowercase())
                                {
                                    debug!(addr = %addr_str, "Found address in logs");
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Check transactions (least efficient, check last)
        if let Some(transactions) =
            json.get("diff").and_then(|d| d.get("transactions")).and_then(|t| t.as_array())
        {
            for tx in transactions {
                if let Some(tx_str) = tx.as_str() {
                    let tx_lower = tx_str.to_lowercase();
                    for address in addresses {
                        if tx_lower.contains(address) {
                            debug!(tx = %tx_str, "Found address in transaction");
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    fn contains_any_topic(&self, json: &Value, topics: &HashSet<String>) -> bool {
        // Check logs in receipts for topics
        if let Some(receipts) =
            json.get("metadata").and_then(|m| m.get("receipts")).and_then(|r| r.as_object())
        {
            for receipt_value in receipts.values() {
                if let Some(receipt_obj) = receipt_value.as_object() {
                    for receipt_data in receipt_obj.values() {
                        if let Some(logs) =
                            receipt_data.get("logs").and_then(|logs| logs.as_array())
                        {
                            for log in logs {
                                if let Some(log_topics) =
                                    log.get("topics").and_then(|topics| topics.as_array())
                                {
                                    for topic_value in log_topics {
                                        if let Some(topic_str) = topic_value.as_str()
                                            && topics.contains(&topic_str.to_lowercase())
                                        {
                                            debug!(topic = %topic_str, "Found topic in logs");
                                            return true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_test_payload() -> Vec<u8> {
        let data = r#"
  {
    "payload_id": "0x0307de8ff1df8ed8",
    "index": 0,
    "metadata": {
      "block_number": 26600873,
      "new_account_balances": {
        "0x4200000000000000000000000000000000000010": "0x13fbe85edc90000"
      },
      "receipts": {
        "0x3fb39b336c13a09d04a34f72cd88a7b0066d65dcf246288ac5bdbba33376eb41": {
          "Deposit": {
            "logs": [
              {
                "address": "0x4200000000000000000000000000000000000010",
                "topics": [
                  "0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89",
                  "0x0000000000000000000000000000000000000000000000000000000000000000"
                ]
              }
            ]
          }
        }
      }
    }
  }
"#;
        data.as_bytes().to_vec()
    }

    #[test]
    fn test_multiple_addresses_filter() {
        let payload = get_test_payload();

        // Test with multiple addresses, one should match
        let addresses = vec![
            "0x1111111111111111111111111111111111111111".to_string(),
            "0x4200000000000000000000000000000000000010".to_string(),
        ];
        let filter = FilterType::new_addresses(addresses).unwrap();
        assert!(filter.matches(&payload, false));

        // Test with multiple addresses, none should match
        let addresses = vec![
            "0x1111111111111111111111111111111111111111".to_string(),
            "0x2222222222222222222222222222222222222222".to_string(),
        ];
        let filter = FilterType::new_addresses(addresses).unwrap();
        assert!(!filter.matches(&payload, false));
    }

    #[test]
    fn test_multiple_topics_filter() {
        let payload = get_test_payload();

        // Test with multiple topics, one should match
        let topics = vec![
            "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
            "0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89".to_string(),
        ];
        let filter = FilterType::new_topics(topics).unwrap();
        assert!(filter.matches(&payload, false));
    }

    #[test]
    fn test_combined_filter_any_mode() {
        let payload = get_test_payload();

        // Test combined filter with ANY mode where both address and topic match (should pass)
        let addresses = vec!["0x4200000000000000000000000000000000000010".to_string()];
        let topics =
            vec!["0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::Any).unwrap();
        assert!(filter.matches(&payload, false));

        // Test combined filter with ANY mode where only address matches (should pass)
        let addresses = vec!["0x4200000000000000000000000000000000000010".to_string()];
        let topics =
            vec!["0x1111111111111111111111111111111111111111111111111111111111111111".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::Any).unwrap();
        assert!(filter.matches(&payload, false));

        // Test combined filter with ANY mode where only topic matches (should pass)
        let addresses = vec!["0x1111111111111111111111111111111111111111".to_string()];
        let topics =
            vec!["0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::Any).unwrap();
        assert!(filter.matches(&payload, false));

        // Test combined filter with ANY mode where neither matches (should fail)
        let addresses = vec!["0x1111111111111111111111111111111111111111".to_string()];
        let topics =
            vec!["0x1111111111111111111111111111111111111111111111111111111111111111".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::Any).unwrap();
        assert!(!filter.matches(&payload, false));
    }

    #[test]
    fn test_combined_filter_all_mode() {
        let payload = get_test_payload();

        // Test combined filter with ALL mode where both address and topic match (should pass)
        let addresses = vec!["0x4200000000000000000000000000000000000010".to_string()];
        let topics =
            vec!["0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::All).unwrap();
        assert!(filter.matches(&payload, false));

        // Test combined filter with ALL mode where only address matches (should fail)
        let addresses = vec!["0x4200000000000000000000000000000000000010".to_string()];
        let topics =
            vec!["0x1111111111111111111111111111111111111111111111111111111111111111".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::All).unwrap();
        assert!(!filter.matches(&payload, false));

        // Test combined filter with ALL mode where only topic matches (should fail)
        let addresses = vec!["0x1111111111111111111111111111111111111111".to_string()];
        let topics =
            vec!["0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::All).unwrap();
        assert!(!filter.matches(&payload, false));
    }

    #[test]
    fn test_with_real_data() {
        // Test against real flashblocks payload data structure
        let payload = r#"
  {
    "payload_id": "0x0307de8ff1df8ed8",
    "index": 0,
    "diff": {
      "transactions": [
        "0x7ef90104a0799b8b5182a2612920c032590217fd987cdcf1e07a2de17907e02eea535cc30694deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be0000044d000a118b000000000000000000000000683f28fc0000000000813aea000000000000000000000000000000000000000000000000000000000000094a0000000000000000000000000000000000000000000000000000000000000001f10c9d7f8fab954891476f8daa9189f45ee736b02bc43cb190e4f891c82e7edf000000000000000000000000fc56e7272eebbba5bc6c544e159483c4a38f8ba3000000000000000000000000"
      ]
    },
    "metadata": {
      "block_number": 26600873,
      "new_account_balances": {
        "0x336f495c2d3d764f541426228178a2369c9b78db": "0x13fbe85edc90000",
        "0x4200000000000000000000000000000000000007": "0xf61bc4ad468f1bd"
      },
      "receipts": {
        "0x3fb39b336c13a09d04a34f72cd88a7b0066d65dcf246288ac5bdbba33376eb41": {
          "Deposit": {
            "logs": [
              {
                "address": "0x4200000000000000000000000000000000000010",
                "topics": [
                  "0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89",
                  "0x0000000000000000000000000000000000000000000000000000000000000000"
                ]
              }
            ]
          }
        }
      }
    }
  }
"#.to_string().into_bytes();

        // Test address filter that should match (in logs)
        let filter = FilterType::new_addresses(vec![
            "0x4200000000000000000000000000000000000010".to_string(),
        ])
        .unwrap();
        assert!(filter.matches(&payload, false));

        // Test address filter that should match (in account balances)
        let filter = FilterType::new_addresses(vec![
            "0x4200000000000000000000000000000000000007".to_string(),
        ])
        .unwrap();
        assert!(filter.matches(&payload, false));

        // Test address filter that should not match
        let filter = FilterType::new_addresses(vec![
            "0x1111111111111111111111111111111111111111".to_string(),
        ])
        .unwrap();
        assert!(!filter.matches(&payload, false));

        // Test topic filter that should match
        let filter = FilterType::new_topics(vec![
            "0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89".to_string(),
        ])
        .unwrap();
        assert!(filter.matches(&payload, false));

        // Test topic filter that should not match
        let filter = FilterType::new_topics(vec![
            "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        ])
        .unwrap();
        assert!(!filter.matches(&payload, false));
    }

    /// Regression test for the filter CPU denial-of-service (CWE-400): an
    /// attacker-controlled filter must not be able to declare an unbounded number
    /// of entries. Before the fix, a `?addresses=z0,z1,...` query with tens of
    /// thousands of tokens built a filter whose per-message match took seconds,
    /// monopolizing a runtime worker and stalling the fan-out for every client.
    ///
    /// Construction must reject any list exceeding [`MAX_FILTER_ENTRIES`].
    #[test]
    fn oversized_filter_is_rejected() {
        let too_many = MAX_FILTER_ENTRIES + 1;
        let addresses: Vec<String> = (0..too_many).map(|i| format!("addr{i}")).collect();
        assert!(matches!(
            FilterType::new_addresses(addresses),
            Err(FilterError::TooManyEntries { kind: "addresses", .. })
        ));

        let topics: Vec<String> = (0..too_many).map(|i| format!("topic{i}")).collect();
        assert!(matches!(
            FilterType::new_topics(topics),
            Err(FilterError::TooManyEntries { kind: "topics", .. })
        ));

        // The combined constructor guards both lists.
        let addresses: Vec<String> = (0..too_many).map(|i| format!("addr{i}")).collect();
        assert!(FilterType::new_combined_with_mode(addresses, vec![], MatchMode::Any).is_err());

        // A filter at the limit is still accepted.
        let at_limit: Vec<String> = (0..MAX_FILTER_ENTRIES).map(|i| format!("addr{i}")).collect();
        assert!(FilterType::new_addresses(at_limit).is_ok());
    }

    /// Worst-case latency regression test for the filter CPU denial-of-service
    /// (CWE-400). `matches()` runs synchronously on a runtime worker for every
    /// upstream flashblock (~every 200ms), so a single match must complete within
    /// that interval or one connection alone falls behind and starves its worker.
    ///
    /// With the entry count bounded to [`MAX_FILTER_ENTRIES`], even the largest
    /// permitted filter against a worst-case flashblock stays well within budget.
    /// (Before the bound, a ~50k-address filter took ~3s here — ~15x over.)
    #[test]
    fn max_permitted_filter_match_stays_within_flashblock_interval() {
        use std::time::{Duration, Instant};

        const FLASHBLOCK_INTERVAL: Duration = Duration::from_millis(200);
        // Worst-case flashblock: many transactions, each a long raw-tx hex string.
        const TRANSACTIONS: usize = 200;
        const TX_HEX_LEN: usize = 1024;

        let tx = format!("0x{}", "0123456789abcdef".repeat(TX_HEX_LEN / 16));
        let transactions: Vec<String> = (0..TRANSACTIONS).map(|_| tx.clone()).collect();
        let flashblock = serde_json::json!({ "diff": { "transactions": transactions } }).to_string();

        // The maximum permitted filter, of non-matching entries so the scan never
        // short-circuits (the attacker's worst case within the bound).
        let addresses: Vec<String> =
            (0..MAX_FILTER_ENTRIES).map(|i| format!("zzzz{i:036}")).collect();
        let filter = FilterType::new_addresses(addresses).unwrap();

        let start = Instant::now();
        let matched = filter.matches(flashblock.as_bytes(), false);
        let elapsed = start.elapsed();
        assert!(!matched, "sanity: junk tokens must not match");

        assert!(
            elapsed < FLASHBLOCK_INTERVAL,
            "largest permitted filter ({MAX_FILTER_ENTRIES} entries) over a {TRANSACTIONS}-tx \
             flashblock took {elapsed:?}, exceeding the {FLASHBLOCK_INTERVAL:?} flashblock interval",
        );
    }
}
