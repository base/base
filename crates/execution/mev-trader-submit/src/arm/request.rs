//! Pure request-spec builders. These turn an [`AuthorizedSignedSubmission`] into
//! the two channels' JSON-RPC bodies. There is NO network here — a [`RequestSpec`]
//! is inert data. The compile-pinned endpoints are private and used only by the
//! builders (and, at send time, by the single live-egress backend).

use alloy_primitives::hex;

use super::witness::AuthorizedSignedSubmission;

/// The Base node JSON-RPC endpoint (inclusion channel → our node → sequencer).
pub(crate) const BASE_NODE_RPC: &str = "http://127.0.0.1:8545";

/// The Blink OFA auction host (attribution channel).
pub(crate) const BLINK_AUCTION_HOST: &str = "https://baseauction.blinklabs.xyz/v1/";

/// Which channel a [`RequestSpec`] targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Inclusion: `eth_sendRawTransaction` of the backrun (priority == victim's).
    Inclusion,
    /// Attribution: `eth_sendBundle[victim_hash, rawBackrun]`, `bidWei = "0"`.
    Attribution,
}

/// An inert, fully-built request: the target channel/endpoint/method plus the
/// exact JSON-RPC body bytes. Carries no capability and opens no socket.
#[derive(Debug, Clone)]
pub struct RequestSpec {
    channel: Channel,
    endpoint: &'static str,
    method: &'static str,
    body: Vec<u8>,
}

impl RequestSpec {
    /// The channel.
    pub const fn channel(&self) -> Channel {
        self.channel
    }
    /// The target endpoint.
    pub const fn endpoint(&self) -> &'static str {
        self.endpoint
    }
    /// The JSON-RPC method.
    pub const fn method(&self) -> &'static str {
        self.method
    }
    /// The exact JSON-RPC body bytes.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    #[cfg(test)]
    pub(crate) fn for_simulation_store_test(channel: Channel) -> Self {
        let method = match channel {
            Channel::Inclusion => "eth_sendRawTransaction",
            Channel::Attribution => "eth_sendBundle",
        };
        Self { channel, endpoint: BASE_NODE_RPC, method, body: Vec::new() }
    }
}
fn encode_body(body: &serde_json::Value) -> Vec<u8> {
    let encoded =
        serde_json::to_vec(body).expect("serializing an internally built JSON value cannot fail");
    debug_assert!(!encoded.is_empty(), "a JSON value always has a nonempty encoding");
    encoded
}
fn inclusion_body(raw_hex: String) -> Vec<u8> {
    encode_body(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_sendRawTransaction",
        "params": [raw_hex],
    }))
}

fn attribution_body(victim_hex: String, backrun_hex: String) -> Vec<u8> {
    encode_body(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_sendBundle",
        "params": [{
            "txs": [victim_hex, backrun_hex],
            "bidWei": "0",
        }],
    }))
}

/// Build the inclusion-channel `eth_sendRawTransaction` request for the backrun.
pub(crate) fn build_inclusion(subm: &AuthorizedSignedSubmission) -> RequestSpec {
    let raw_hex = hex::encode_prefixed(subm.raw_tx().as_ref());
    RequestSpec {
        channel: Channel::Inclusion,
        endpoint: BASE_NODE_RPC,
        method: "eth_sendRawTransaction",
        body: inclusion_body(raw_hex),
    }
}

/// Build the attribution-channel `eth_sendBundle[victim_hash, rawBackrun]` request
/// with `bidWei = "0"` (Blink OFA attribution ignores the bid).
pub(crate) fn build_attribution(subm: &AuthorizedSignedSubmission) -> RequestSpec {
    let victim_hex = hex::encode_prefixed(subm.victim().as_slice());
    let backrun_hex = hex::encode_prefixed(subm.raw_tx().as_ref());
    RequestSpec {
        channel: Channel::Attribution,
        endpoint: BLINK_AUCTION_HOST,
        method: "eth_sendBundle",
        body: attribution_body(victim_hex, backrun_hex),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inclusion_body_has_exact_nonempty_json_rpc_schema() {
        let encoded = inclusion_body("0x0102".to_owned());

        assert!(!encoded.is_empty());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encoded).expect("valid JSON"),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_sendRawTransaction",
                "params": ["0x0102"],
            }),
        );
    }

    #[test]
    fn attribution_body_has_exact_nonempty_ordered_bundle_schema_and_zero_bid() {
        let encoded = attribution_body("0xvictim".to_owned(), "0xbackrun".to_owned());

        assert!(!encoded.is_empty());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encoded).expect("valid JSON"),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_sendBundle",
                "params": [{
                    "txs": ["0xvictim", "0xbackrun"],
                    "bidWei": "0",
                }],
            }),
        );
    }
}
