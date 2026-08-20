//! Types related to logs for Base chains.

use alloy_primitives::Log as PrimitiveLog;
use alloy_rpc_types_eth::Log;
use serde::{Deserialize, Serialize};

/// Base log response with an optional full block timestamp in milliseconds.
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    derive_more::Deref,
    derive_more::DerefMut,
)]
#[serde(rename_all = "camelCase")]
pub struct BaseLogResponse {
    /// Standard Ethereum log response.
    #[deref]
    #[deref_mut]
    #[serde(flatten)]
    pub inner: Log,
    /// Full Unix timestamp in milliseconds when sub-second timing is available.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub block_timestamp_ms: Option<u64>,
}

impl AsRef<PrimitiveLog> for BaseLogResponse {
    fn as_ref(&self) -> &PrimitiveLog {
        self.inner.as_ref()
    }
}

impl From<Log> for BaseLogResponse {
    fn from(inner: Log) -> Self {
        Self { inner, block_timestamp_ms: None }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn block_timestamp_ms_is_optional_quantity() {
        let log =
            BaseLogResponse { block_timestamp_ms: Some(1_700_000_000_200), ..Default::default() };

        let value = serde_json::to_value(&log).unwrap();
        assert_eq!(value["blockTimestampMs"], "0x18bcfe568c8");

        let round_trip: BaseLogResponse = serde_json::from_value(value).unwrap();
        assert_eq!(round_trip.block_timestamp_ms, Some(1_700_000_000_200));

        assert_eq!(
            serde_json::to_value(BaseLogResponse::default()).unwrap(),
            json!({
                "address": "0x0000000000000000000000000000000000000000",
                "topics": [],
                "data": "0x",
                "blockHash": null,
                "blockNumber": null,
                "transactionHash": null,
                "transactionIndex": null,
                "logIndex": null,
                "removed": false
            })
        );
    }
}
