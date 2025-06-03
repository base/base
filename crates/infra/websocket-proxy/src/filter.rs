use brotli::DecompressorWriter;
use serde_json::{self, Value};
use std::collections::HashSet;
use std::io::Write;
use tracing::{debug, info, trace, warn};

#[derive(Debug, Clone, Copy, Default)]
pub enum MatchMode {
    #[default]
    Any, // OR logic - match if any condition is true
    All, // AND logic - match only if all conditions are true
}

#[derive(Debug, Clone)]
pub enum FilterType {
    Addresses(HashSet<String>),
    Topics(HashSet<String>),
    Combined {
        addresses: HashSet<String>,
        topics: HashSet<String>,
        match_mode: MatchMode,
    },
    None,
}

impl FilterType {
    pub fn new_addresses(addresses: Vec<String>) -> Self {
        if addresses.is_empty() {
            Self::None
        } else {
            let normalized: HashSet<String> = addresses
                .into_iter()
                .map(|addr| addr.to_lowercase())
                .collect();
            Self::Addresses(normalized)
        }
    }

    pub fn new_topics(topics: Vec<String>) -> Self {
        if topics.is_empty() {
            Self::None
        } else {
            let normalized: HashSet<String> = topics
                .into_iter()
                .map(|topic| topic.to_lowercase())
                .collect();
            Self::Topics(normalized)
        }
    }

    pub fn new_combined_with_mode(
        addresses: Vec<String>,
        topics: Vec<String>,
        match_mode: MatchMode,
    ) -> Self {
        if addresses.is_empty() && topics.is_empty() {
            Self::None
        } else if addresses.is_empty() {
            Self::new_topics(topics)
        } else if topics.is_empty() {
            Self::new_addresses(addresses)
        } else {
            let normalized_addresses: HashSet<String> = addresses
                .into_iter()
                .map(|addr| addr.to_lowercase())
                .collect();
            let normalized_topics: HashSet<String> = topics
                .into_iter()
                .map(|topic| topic.to_lowercase())
                .collect();
            Self::Combined {
                addresses: normalized_addresses,
                topics: normalized_topics,
                match_mode,
            }
        }
    }

    pub fn matches(&self, payload: &[u8], enable_compression: bool) -> bool {
        if let FilterType::None = self {
            return true;
        }

        let uncompressed_data = if enable_compression {
            let mut uncompressed_bytes = Vec::new();
            {
                let mut decoder = DecompressorWriter::new(&mut uncompressed_bytes, 4096);
                match decoder.write_all(payload) {
                    Ok(_) => (),
                    Err(e) => {
                        info!("error while decoding payload: {}", e);
                        return false;
                    }
                }
            }
            uncompressed_bytes
        } else {
            payload.to_owned()
        };

        let value = String::from_utf8(uncompressed_data);
        if value.is_err() {
            return false;
        }

        let json_result: Result<Value, _> = serde_json::from_str(value.unwrap().as_str());
        match json_result {
            Ok(json) => {
                let result = self.json_matches(&json);
                trace!("Filter result: {} for filter type: {:?}", result, self);
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
            FilterType::Addresses(addresses) => self.contains_any_address(json, addresses),
            FilterType::Topics(topics) => self.contains_any_topic(json, topics),
            FilterType::Combined {
                addresses,
                topics,
                match_mode,
            } => {
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
            FilterType::None => true,
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
                    debug!("Found address in new_account_balances: {}", account);
                    return true;
                }
            }
        }

        // Check logs in receipts (most common case for filtering)
        if let Some(receipts) = json
            .get("metadata")
            .and_then(|m| m.get("receipts"))
            .and_then(|r| r.as_object())
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
                                {
                                    if addresses.contains(&addr_str.to_lowercase()) {
                                        debug!("Found address in logs: {}", addr_str);
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Check transactions (least efficient, check last)
        if let Some(transactions) = json
            .get("diff")
            .and_then(|d| d.get("transactions"))
            .and_then(|t| t.as_array())
        {
            for tx in transactions {
                if let Some(tx_str) = tx.as_str() {
                    let tx_lower = tx_str.to_lowercase();
                    for address in addresses {
                        if tx_lower.contains(address) {
                            debug!("Found address in transaction: {}", tx_str);
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
        if let Some(receipts) = json
            .get("metadata")
            .and_then(|m| m.get("receipts"))
            .and_then(|r| r.as_object())
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
                                        if let Some(topic_str) = topic_value.as_str() {
                                            if topics.contains(&topic_str.to_lowercase()) {
                                                debug!("Found topic in logs: {}", topic_str);
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
        let filter = FilterType::new_addresses(addresses);
        assert!(filter.matches(&payload, false));

        // Test with multiple addresses, none should match
        let addresses = vec![
            "0x1111111111111111111111111111111111111111".to_string(),
            "0x2222222222222222222222222222222222222222".to_string(),
        ];
        let filter = FilterType::new_addresses(addresses);
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
        let filter = FilterType::new_topics(topics);
        assert!(filter.matches(&payload, false));
    }

    #[test]
    fn test_combined_filter_any_mode() {
        let payload = get_test_payload();

        // Test combined filter with ANY mode where both address and topic match (should pass)
        let addresses = vec!["0x4200000000000000000000000000000000000010".to_string()];
        let topics =
            vec!["0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::Any);
        assert!(filter.matches(&payload, false));

        // Test combined filter with ANY mode where only address matches (should pass)
        let addresses = vec!["0x4200000000000000000000000000000000000010".to_string()];
        let topics =
            vec!["0x1111111111111111111111111111111111111111111111111111111111111111".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::Any);
        assert!(filter.matches(&payload, false));

        // Test combined filter with ANY mode where only topic matches (should pass)
        let addresses = vec!["0x1111111111111111111111111111111111111111".to_string()];
        let topics =
            vec!["0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::Any);
        assert!(filter.matches(&payload, false));

        // Test combined filter with ANY mode where neither matches (should fail)
        let addresses = vec!["0x1111111111111111111111111111111111111111".to_string()];
        let topics =
            vec!["0x1111111111111111111111111111111111111111111111111111111111111111".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::Any);
        assert!(!filter.matches(&payload, false));
    }

    #[test]
    fn test_combined_filter_all_mode() {
        let payload = get_test_payload();

        // Test combined filter with ALL mode where both address and topic match (should pass)
        let addresses = vec!["0x4200000000000000000000000000000000000010".to_string()];
        let topics =
            vec!["0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::All);
        assert!(filter.matches(&payload, false));

        // Test combined filter with ALL mode where only address matches (should fail)
        let addresses = vec!["0x4200000000000000000000000000000000000010".to_string()];
        let topics =
            vec!["0x1111111111111111111111111111111111111111111111111111111111111111".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::All);
        assert!(!filter.matches(&payload, false));

        // Test combined filter with ALL mode where only topic matches (should fail)
        let addresses = vec!["0x1111111111111111111111111111111111111111".to_string()];
        let topics =
            vec!["0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89".to_string()];
        let filter = FilterType::new_combined_with_mode(addresses, topics, MatchMode::All);
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
            "0x4200000000000000000000000000000000000010".to_string()
        ]);
        assert!(filter.matches(&payload, false));

        // Test address filter that should match (in account balances)
        let filter = FilterType::new_addresses(vec![
            "0x4200000000000000000000000000000000000007".to_string()
        ]);
        assert!(filter.matches(&payload, false));

        // Test address filter that should not match
        let filter = FilterType::new_addresses(vec![
            "0x1111111111111111111111111111111111111111".to_string()
        ]);
        assert!(!filter.matches(&payload, false));

        // Test topic filter that should match
        let filter = FilterType::new_topics(vec![
            "0xb0444523268717a02698be47d0803aa7468c00acbed2f8bd93a0459cde61dd89".to_string(),
        ]);
        assert!(filter.matches(&payload, false));

        // Test topic filter that should not match
        let filter = FilterType::new_topics(vec![
            "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        ]);
        assert!(!filter.matches(&payload, false));
    }
}
