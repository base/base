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

impl FilterType {
    /// Creates an address-only filter, or [`FilterType::None`] if the list is empty.
    pub fn new_addresses(addresses: Vec<String>) -> Self {
        if addresses.is_empty() {
            Self::None
        } else {
            let normalized: HashSet<String> =
                addresses.into_iter().map(|addr| addr.to_lowercase()).collect();
            Self::Addresses(normalized)
        }
    }

    /// Creates a topic-only filter, or [`FilterType::None`] if the list is empty.
    pub fn new_topics(topics: Vec<String>) -> Self {
        if topics.is_empty() {
            Self::None
        } else {
            let normalized: HashSet<String> =
                topics.into_iter().map(|topic| topic.to_lowercase()).collect();
            Self::Topics(normalized)
        }
    }

    /// Creates a combined address-and-topic filter with the specified [`MatchMode`].
    pub fn new_combined_with_mode(
        addresses: Vec<String>,
        topics: Vec<String>,
        match_mode: MatchMode,
    ) -> Self {
        if addresses.is_empty() {
            return Self::new_topics(topics);
        }
        if topics.is_empty() {
            return Self::new_addresses(addresses);
        }

        Self::Combined {
            addresses: addresses.into_iter().map(|addr| addr.to_lowercase()).collect(),
            topics: topics.into_iter().map(|topic| topic.to_lowercase()).collect(),
            match_mode,
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
        // Check logs in receipts first (most common case for filtering)
        let in_logs = Self::logs(json)
            .filter_map(|log| log.get("address").and_then(Value::as_str))
            .find(|addr| addresses.contains(&addr.to_lowercase()));
        if let Some(addr) = in_logs {
            debug!(addr = %addr, "Found address in logs");
            return true;
        }

        // Check transactions (least efficient, check last)
        let in_transactions = json
            .get("transactions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|tx| {
                let tx = tx.to_lowercase();
                addresses.iter().any(|address| tx.contains(address.as_str()))
            });
        if in_transactions {
            debug!("Found address in transactions");
        }
        in_transactions
    }

    fn contains_any_topic(&self, json: &Value, topics: &HashSet<String>) -> bool {
        let found = Self::logs(json)
            .filter_map(|log| log.get("topics").and_then(Value::as_array))
            .flatten()
            .filter_map(Value::as_str)
            .find(|topic| topics.contains(&topic.to_lowercase()));
        if let Some(topic) = found {
            debug!(topic = %topic, "Found topic in logs");
        }
        found.is_some()
    }

    /// Iterates every log across the receipts of a block stream message.
    fn logs(json: &Value) -> impl Iterator<Item = &Value> {
        json.get("receipts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|receipt| receipt.get("logs").and_then(Value::as_array))
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_test_payload() -> Vec<u8> {
        let data = r#"
  {
    "blockNumber": 26600873,
    "header": { "number": "0x195e4a9" },
    "transactions": [],
    "receipts": [
      {
        "type": "0x7e",
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
    ]
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
}
