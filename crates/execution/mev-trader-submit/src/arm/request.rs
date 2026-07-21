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
}

/// Build the inclusion-channel `eth_sendRawTransaction` request for the backrun.
pub(crate) fn build_inclusion(subm: &AuthorizedSignedSubmission) -> RequestSpec {
    let raw_hex = hex::encode_prefixed(subm.raw_tx().as_ref());
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_sendRawTransaction",
        "params": [raw_hex],
    });
    RequestSpec {
        channel: Channel::Inclusion,
        endpoint: BASE_NODE_RPC,
        method: "eth_sendRawTransaction",
        body: serde_json::to_vec(&body).unwrap_or_default(),
    }
}

/// Build the attribution-channel `eth_sendBundle[victim_hash, rawBackrun]` request
/// with `bidWei = "0"` (Blink OFA attribution ignores the bid).
pub(crate) fn build_attribution(subm: &AuthorizedSignedSubmission) -> RequestSpec {
    let victim_hex = hex::encode_prefixed(subm.victim().as_slice());
    let backrun_hex = hex::encode_prefixed(subm.raw_tx().as_ref());
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_sendBundle",
        "params": [{
            "txs": [victim_hex, backrun_hex],
            "bidWei": "0",
        }],
    });
    RequestSpec {
        channel: Channel::Attribution,
        endpoint: BLINK_AUCTION_HOST,
        method: "eth_sendBundle",
        body: serde_json::to_vec(&body).unwrap_or_default(),
    }
}
