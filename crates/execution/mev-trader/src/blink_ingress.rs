use std::{
    ffi::OsString,
    fmt,
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    str::FromStr,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, Bytes};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::{sync::Notify, time};
#[cfg(not(test))]
use tokio_tungstenite::connect_async_tls_with_config;
#[cfg(test)]
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::{
    Error as WebSocketError,
    client::IntoClientRequest,
    error::ProtocolError,
    protocol::{Message, WebSocketConfig},
};

use crate::MevTraderRuntime;
#[cfg(feature = "edge-measurement")]
use crate::{BlinkRejectClassifierV3, BlinkRejectReasonV3};

const BLINK_ENDPOINT: &str = "wss://baseauction.blinklabs.xyz/ws/v1/";
const BLINK_SUBSCRIBE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["blink_partialPendingTransactions"]}"#;
const MAX_WIRE_BYTES: usize = 1_048_576;
const MAX_RAW_TX_BYTES: usize = 131_072;
const MAX_CREDENTIAL_BYTES: usize = 256;
const MAX_SAFE_JSON_INTEGER: u64 = 9_007_199_254_740_991;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(8);
const A1_OUTCOME_COUNT: usize = 14;

/// Opaque receive-only Blink credential.
pub struct BlinkCredential {
    bytes: [u8; MAX_CREDENTIAL_BYTES],
    len: usize,
}

impl fmt::Debug for BlinkCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("BlinkCredential").field("value", &"[REDACTED]").finish()
    }
}

impl BlinkCredential {
    /// Loads one bounded credential from a regular Unix `0600` file.
    pub fn load(path: OsString) -> Option<Self> {
        if path.is_empty() {
            return None;
        }
        let path = PathBuf::from(path);
        path.to_str()?;
        let pre_open = fs::symlink_metadata(&path).ok()?;
        if pre_open.file_type().is_symlink() {
            return None;
        }

        let mut file = File::open(path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.mode() & 0o777 != 0o600 {
            return None;
        }

        let mut value = Vec::with_capacity(MAX_CREDENTIAL_BYTES + 1);
        file.by_ref()
            .take(u64::try_from(MAX_CREDENTIAL_BYTES + 1).ok()?)
            .read_to_end(&mut value)
            .ok()?;
        if value.len() > MAX_CREDENTIAL_BYTES {
            return None;
        }
        if value.ends_with(b"\r\n") {
            value.truncate(value.len() - 2);
        } else if value.ends_with(b"\n") {
            value.truncate(value.len() - 1);
        }
        if value.is_empty()
            || value.len() > MAX_CREDENTIAL_BYTES
            || value.iter().any(|byte| {
                !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'_' | b'~' | b'-')
            })
        {
            return None;
        }

        let mut bytes = [0; MAX_CREDENTIAL_BYTES];
        bytes[..value.len()].copy_from_slice(&value);
        Some(Self { bytes, len: value.len() })
    }
}

/// Opaque post-gate Blink ingress configuration.
#[derive(Debug)]
pub struct BlinkIngressConfig {
    credential_file: OsString,
    #[cfg(test)]
    endpoint: Option<String>,
}

impl BlinkIngressConfig {
    /// Creates the production configuration with the fixed Blink endpoint.
    pub const fn production(credential_file: OsString) -> Self {
        Self {
            credential_file,
            #[cfg(test)]
            endpoint: None,
        }
    }
}

/// Feed-observable victim fields plus local monotonic receive time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlinkVictim {
    block_number: u64,
    flashblock_index: u64,
    chain_id: u64,
    transaction_type: u8,
    hash: B256,
    from: Address,
    raw_tx: Bytes,
    received_at: Instant,
}

impl BlinkVictim {
    /// Returns the feed-authored block number.
    pub const fn block_number(&self) -> u64 {
        self.block_number
    }

    /// Returns the feed-authored victim flashblock index.
    pub const fn flashblock_index(&self) -> u64 {
        self.flashblock_index
    }

    /// Returns the feed-authored chain identifier.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Returns the feed-authored EIP-2718 transaction type.
    pub const fn transaction_type(&self) -> u8 {
        self.transaction_type
    }

    /// Returns the feed-authored transaction hash.
    pub const fn hash(&self) -> B256 {
        self.hash
    }

    /// Returns the feed-authored transaction sender.
    pub const fn from(&self) -> Address {
        self.from
    }

    /// Returns the bounded raw transaction bytes.
    pub const fn raw_tx(&self) -> &Bytes {
        &self.raw_tx
    }

    /// Returns the local monotonic receive time.
    pub const fn received_at(&self) -> Instant {
        self.received_at
    }

    pub(crate) fn decode(
        text: &str,
        subscription: &str,
        received_at: Instant,
    ) -> Result<Self, A1Outcome> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Notification {
            jsonrpc: String,
            method: String,
            params: NotificationParams,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct NotificationParams {
            subscription: String,
            timestamp: u64,
            publish_time: u64,
            block_number: String,
            flashblock_index: String,
            result: NotificationResult,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct NotificationResult {
            chain_id: String,
            #[serde(rename = "type")]
            transaction_type: String,
            hash: String,
            from: String,
            raw_tx: String,
        }

        let value: Value = serde_json::from_str(text).map_err(|_| A1Outcome::ApplicationDrop)?;
        let params = value.get("params").and_then(Value::as_object);
        let valid_envelope = value.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
            && value.get("method").and_then(Value::as_str) == Some("eth_subscription")
            && params.and_then(|params| params.get("subscription")).and_then(Value::as_str)
                == Some(subscription);
        if !valid_envelope {
            return Err(A1Outcome::ProtocolDisabled);
        }

        let notification: Notification =
            serde_json::from_value(value).map_err(|_| A1Outcome::ApplicationDrop)?;
        if notification.jsonrpc != "2.0"
            || notification.method != "eth_subscription"
            || notification.params.subscription != subscription
            || notification.params.timestamp > MAX_SAFE_JSON_INTEGER
            || notification.params.publish_time > MAX_SAFE_JSON_INTEGER
        {
            return Err(A1Outcome::ProtocolDisabled);
        }

        let block_number = Self::parse_quantity(&notification.params.block_number)
            .ok_or(A1Outcome::ApplicationDrop)?;
        let flashblock_index = Self::parse_quantity(&notification.params.flashblock_index)
            .ok_or(A1Outcome::ApplicationDrop)?;
        let chain_id = Self::parse_quantity(&notification.params.result.chain_id)
            .ok_or(A1Outcome::ApplicationDrop)?;
        let transaction_type = u8::try_from(
            Self::parse_quantity(&notification.params.result.transaction_type)
                .ok_or(A1Outcome::ApplicationDrop)?,
        )
        .map_err(|_| A1Outcome::ApplicationDrop)?;
        let hash_text = &notification.params.result.hash;
        if hash_text.len() != 66
            || !hash_text.starts_with("0x")
            || !hash_text[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(A1Outcome::ApplicationDrop);
        }
        let hash = B256::from_str(hash_text).map_err(|_| A1Outcome::ApplicationDrop)?;
        let from_text = &notification.params.result.from;
        if from_text.len() != 42
            || !from_text.starts_with("0x")
            || !from_text[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(A1Outcome::ApplicationDrop);
        }
        let from = Address::from_str(from_text).map_err(|_| A1Outcome::ApplicationDrop)?;
        let raw_text = &notification.params.result.raw_tx;
        let raw_hex = raw_text.strip_prefix("0x").ok_or(A1Outcome::ApplicationDrop)?;
        if raw_hex.is_empty()
            || raw_hex.len() % 2 != 0
            || raw_hex.len() / 2 > MAX_RAW_TX_BYTES
            || !raw_hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(A1Outcome::ApplicationDrop);
        }
        let raw_tx = Bytes::from_str(raw_text).map_err(|_| A1Outcome::ApplicationDrop)?;

        Ok(Self {
            block_number,
            flashblock_index,
            chain_id,
            transaction_type,
            hash,
            from,
            raw_tx,
            received_at,
        })
    }

    fn parse_quantity(value: &str) -> Option<u64> {
        let digits = value.strip_prefix("0x")?;
        if digits.is_empty()
            || (digits.len() > 1 && digits.starts_with('0'))
            || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        u64::from_str_radix(digits, 16).ok()
    }

    #[cfg(feature = "edge-measurement")]
    fn classify_decode_rejection(
        text: &str,
        subscription: &str,
        disposition: A1Outcome,
    ) -> BlinkRejectReasonV3 {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return BlinkRejectReasonV3::JsonSyntax;
        };
        let Some(root) = value.as_object() else {
            return BlinkRejectReasonV3::RootWrongType;
        };
        if root.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return BlinkRejectReasonV3::JsonRpcMismatch;
        }
        if root.get("method").and_then(Value::as_str) != Some("eth_subscription") {
            return BlinkRejectReasonV3::MethodMismatch;
        }
        let Some(params) = root.get("params").and_then(Value::as_object) else {
            return BlinkRejectReasonV3::ParamsInvalid;
        };
        if params.get("subscription").and_then(Value::as_str) != Some(subscription) {
            return BlinkRejectReasonV3::SubscriptionMismatch;
        }
        if params
            .get("timestamp")
            .and_then(Value::as_u64)
            .is_none_or(|value| value > MAX_SAFE_JSON_INTEGER)
        {
            return BlinkRejectReasonV3::TimestampUnsafe;
        }
        if params
            .get("publishTime")
            .and_then(Value::as_u64)
            .is_none_or(|value| value > MAX_SAFE_JSON_INTEGER)
        {
            return BlinkRejectReasonV3::PublishTimeUnsafe;
        }
        let quantity = |name: &str, reason| {
            params
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| Self::parse_quantity(value).is_some())
                .map(|_| ())
                .ok_or(reason)
        };
        if let Err(reason) = quantity("blockNumber", BlinkRejectReasonV3::BlockNumberInvalid) {
            return reason;
        }
        if let Err(reason) =
            quantity("flashblockIndex", BlinkRejectReasonV3::FlashblockIndexInvalid)
        {
            return reason;
        }
        let Some(result) = params.get("result").and_then(Value::as_object) else {
            return BlinkRejectReasonV3::ParamsInvalid;
        };
        let result_quantity = |name: &str, reason| {
            result
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| Self::parse_quantity(value).is_some())
                .map(|_| ())
                .ok_or(reason)
        };
        if let Err(reason) = result_quantity("chainId", BlinkRejectReasonV3::ChainIdInvalid) {
            return reason;
        }
        let transaction_type =
            result.get("type").and_then(Value::as_str).and_then(Self::parse_quantity);
        if transaction_type.and_then(|value| u8::try_from(value).ok()).is_none() {
            return BlinkRejectReasonV3::TransactionTypeInvalid;
        }
        let Some(hash) = result.get("hash").and_then(Value::as_str) else {
            return BlinkRejectReasonV3::TxHashInvalid;
        };
        if hash.len() != 66
            || !hash.starts_with("0x")
            || !hash[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
            || B256::from_str(hash).is_err()
        {
            return BlinkRejectReasonV3::TxHashInvalid;
        }
        let Some(sender) = result.get("from").and_then(Value::as_str) else {
            return BlinkRejectReasonV3::SenderInvalid;
        };
        if sender.len() != 42
            || !sender.starts_with("0x")
            || !sender[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
            || Address::from_str(sender).is_err()
        {
            return BlinkRejectReasonV3::SenderInvalid;
        }
        let Some(raw) = result.get("rawTx").and_then(Value::as_str) else {
            return BlinkRejectReasonV3::RawMissingPrefix;
        };
        let Some(raw_hex) = raw.strip_prefix("0x") else {
            return BlinkRejectReasonV3::RawMissingPrefix;
        };
        if raw_hex.is_empty() {
            return BlinkRejectReasonV3::RawEmpty;
        }
        if raw_hex.len() % 2 != 0 {
            return BlinkRejectReasonV3::RawOddLength;
        }
        if raw_hex.len() / 2 > MAX_RAW_TX_BYTES {
            return BlinkRejectReasonV3::RawOversize;
        }
        if !raw_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return BlinkRejectReasonV3::RawNonHex;
        }
        if Bytes::from_str(raw).is_err() {
            return BlinkRejectReasonV3::RawDecode;
        }
        match disposition {
            A1Outcome::ApplicationDrop => BlinkRejectReasonV3::NotificationApplicationDrop,
            A1Outcome::ProtocolDisabled => BlinkRejectReasonV3::NotificationProtocolDisabled,
            _ => BlinkRejectReasonV3::NotificationInternalFailure,
        }
    }

    #[cfg(feature = "edge-measurement")]
    fn classify_decode_rejection_branch(
        text: &str,
        subscription: &str,
        disposition: A1Outcome,
    ) -> (&'static str, BlinkRejectReasonV3) {
        let reason = Self::classify_decode_rejection(text, subscription, disposition);
        let value = serde_json::from_str::<Value>(text).ok();
        let params = value
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|root| root.get("params"))
            .and_then(Value::as_object);
        let result = params.and_then(|params| params.get("result")).and_then(Value::as_object);
        let branch = match reason {
            BlinkRejectReasonV3::ParamsInvalid if params.is_some() => "decode-result-invalid",
            BlinkRejectReasonV3::ParamsInvalid => "decode-params-invalid",
            BlinkRejectReasonV3::TxHashInvalid
                if result
                    .and_then(|result| result.get("hash"))
                    .and_then(Value::as_str)
                    .is_some() =>
            {
                "decode-transaction-hash-malformed"
            }
            BlinkRejectReasonV3::TxHashInvalid => "decode-transaction-hash-invalid",
            BlinkRejectReasonV3::SenderInvalid
                if result
                    .and_then(|result| result.get("from"))
                    .and_then(Value::as_str)
                    .is_some() =>
            {
                "decode-sender-malformed"
            }
            BlinkRejectReasonV3::SenderInvalid => "decode-sender-invalid",
            BlinkRejectReasonV3::RawMissingPrefix
                if result
                    .and_then(|result| result.get("rawTx"))
                    .and_then(Value::as_str)
                    .is_some() =>
            {
                "decode-raw-prefix-invalid"
            }
            BlinkRejectReasonV3::RawMissingPrefix => "decode-raw-missing-prefix",
            _ => BlinkRejectClassifierV3::branch_id(reason)
                .expect("one-to-one decode reason must have an inventory branch"),
        };
        (branch, reason)
    }
}

/// One generation assigned to a decoded Blink victim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedBlinkVictim {
    generation: u64,
    victim: BlinkVictim,
}

impl QueuedBlinkVictim {
    /// Creates one runtime-owned queued generation.
    pub const fn new(generation: u64, victim: BlinkVictim) -> Self {
        Self { generation, victim }
    }

    /// Returns the checked runtime generation.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the feed-only victim.
    pub const fn victim(&self) -> &BlinkVictim {
        &self.victim
    }

    /// Consumes the queue wrapper into its feed-only victim.
    pub fn into_victim(self) -> BlinkVictim {
        self.victim
    }
}

/// Closed receive-only ingress states.
#[repr(u8)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum A1Status {
    /// Exact default-off state.
    #[default]
    Off = 0,
    /// Credential or post-gate configuration prevents connection.
    DisabledNoConnect = 1,
    /// A bounded connection attempt is active.
    Connecting = 2,
    /// The fixed subscription was sent and acknowledgment is pending.
    AwaitingAck = 3,
    /// Notifications may be decoded into the capacity-one slot.
    Subscribed = 4,
    /// A bounded root-cancellable delay precedes reconnection.
    Retrying = 5,
    /// A protocol or internal failure permanently disabled ingress.
    DisabledPermanent = 6,
    /// Root shutdown closed ingress.
    Closed = 7,
}

impl A1Status {
    /// Decodes the internal atomic representation.
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Off,
            1 => Self::DisabledNoConnect,
            2 => Self::Connecting,
            3 => Self::AwaitingAck,
            4 => Self::Subscribed,
            5 => Self::Retrying,
            6 => Self::DisabledPermanent,
            _ => Self::Closed,
        }
    }

    /// Returns whether the state can never become active again.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::DisabledNoConnect | Self::DisabledPermanent | Self::Closed)
    }
}

/// Closed finite receive and runtime outcomes.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A1Outcome {
    /// One bounded notification decoded and reached slot submission.
    NotificationDecodedSubmitted = 0,
    /// One malformed or unsupported application input was dropped.
    ApplicationDrop = 1,
    /// One protocol/authentication failure permanently disabled ingress.
    ProtocolDisabled = 2,
    /// One automatic WebSocket control message was observed.
    ControlObserved = 3,
    /// One close/reset/end-of-stream was observed.
    DisconnectObserved = 4,
    /// One transient transport failure was observed.
    TransportFailure = 5,
    /// One decoded generation filled the empty slot.
    SlotAccepted = 6,
    /// One decoded generation replaced the queued generation.
    SlotReplaced = 7,
    /// One decoded generation was rejected after lifecycle closure.
    SlotClosed = 8,
    /// One generation successfully bound to an authoritative frame.
    FrameBound = 9,
    /// One completed generation produced no frame-bound trade input.
    NoTrade = 10,
    /// One generation cooperatively acknowledged cancellation.
    Cancelled = 11,
    /// One controlled invariant, panic, or hang failure occurred.
    InternalFailure = 12,
    /// Checked generation assignment overflowed.
    GenerationOverflow = 13,
}

impl A1Outcome {
    /// Returns the fixed counter index.
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Fixed saturating counters with no payload retention.
#[derive(Debug)]
pub struct A1Counters {
    values: [AtomicU64; A1_OUTCOME_COUNT],
}

impl Default for A1Counters {
    fn default() -> Self {
        Self { values: std::array::from_fn(|_| AtomicU64::new(0)) }
    }
}

impl A1Counters {
    /// Saturatingly increments one closed outcome counter.
    pub fn record(&self, outcome: A1Outcome) {
        let counter = &self.values[outcome.index()];
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(1);
            match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Returns one closed outcome count.
    pub fn count(&self, outcome: A1Outcome) -> u64 {
        self.values[outcome.index()].load(Ordering::Relaxed)
    }

    /// Returns a fixed snapshot ordered by [`A1Outcome::index`].
    pub fn snapshot(&self) -> [u64; A1_OUTCOME_COUNT] {
        std::array::from_fn(|index| self.values[index].load(Ordering::Relaxed))
    }
}

/// Root cancellation state with a dedicated wake domain.
#[derive(Debug, Default)]
pub struct RuntimeShutdown {
    cancelled: AtomicBool,
    cancel_notify: Notify,
}

impl RuntimeShutdown {
    /// Irreversibly cancels the root and wakes every armed waiter.
    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::SeqCst) {
            self.cancel_notify.notify_waiters();
        }
    }

    /// Returns the sequentially consistent root state.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Waits without a lost-wake race until root cancellation.
    pub async fn wait_cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.cancel_notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Fixed receive-only WebSocket client with no socket or sink accessor.
#[derive(Debug)]
pub struct BlinkFeedClient {
    credential: BlinkCredential,
    runtime: std::sync::Arc<MevTraderRuntime>,
    #[cfg(test)]
    endpoint: Option<String>,
}

impl BlinkFeedClient {
    /// Loads the credential once and constructs the fixed receive-only client.
    pub fn new(
        config: BlinkIngressConfig,
        runtime: std::sync::Arc<MevTraderRuntime>,
    ) -> Option<Self> {
        let credential = BlinkCredential::load(config.credential_file);
        let Some(credential) = credential else {
            runtime.set_a1_status(A1Status::DisabledNoConnect);
            return None;
        };
        Some(Self {
            credential,
            runtime,
            #[cfg(test)]
            endpoint: config.endpoint,
        })
    }

    /// Runs bounded connect/subscribe/read/retry behavior until a terminal root state.
    pub async fn run(self) {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if self.runtime.shutdown().is_cancelled() {
                self.runtime.set_a1_status(A1Status::Closed);
                return;
            }
            self.runtime.set_a1_status(A1Status::Connecting);

            let mut uri = String::with_capacity(BLINK_ENDPOINT.len() + self.credential.len);
            #[cfg(not(test))]
            uri.push_str(BLINK_ENDPOINT);
            #[cfg(test)]
            uri.push_str(self.endpoint.as_deref().unwrap_or(BLINK_ENDPOINT));
            uri.push_str(
                std::str::from_utf8(&self.credential.bytes[..self.credential.len]).unwrap(),
            );
            let request = match uri.into_client_request() {
                Ok(request) => request,
                Err(_) => {
                    #[cfg(feature = "edge-measurement")]
                    self.runtime
                        .emit_blink_reject("request-build", BlinkRejectReasonV3::RequestBuild);
                    self.runtime.record_a1(A1Outcome::ProtocolDisabled);
                    self.runtime.set_a1_status(A1Status::DisabledPermanent);
                    self.runtime.shutdown().cancel();
                    return;
                }
            };
            let websocket_config = WebSocketConfig::default()
                .max_message_size(Some(MAX_WIRE_BYTES))
                .max_frame_size(Some(MAX_WIRE_BYTES));

            #[cfg(not(test))]
            let connection = time::timeout(
                OPERATION_TIMEOUT,
                connect_async_tls_with_config(request, Some(websocket_config), false, None),
            )
            .await;
            #[cfg(test)]
            let connection = time::timeout(
                OPERATION_TIMEOUT,
                connect_async_with_config(request, Some(websocket_config), false),
            )
            .await;

            let (mut socket, _) = match connection {
                Ok(Ok(connection)) => connection,
                Ok(Err(error)) => {
                    let (status, outcome, retry, cancel_root) =
                        self.classify_error_and_emit(&error);
                    if let Some(outcome) = outcome {
                        self.runtime.record_a1(outcome);
                    }
                    self.runtime.set_a1_status(status);
                    if cancel_root {
                        self.runtime.shutdown().cancel();
                    }
                    if !retry {
                        return;
                    }
                    if !self.wait_backoff(backoff).await {
                        return;
                    }
                    backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
                    continue;
                }
                Err(_) => {
                    #[cfg(feature = "edge-measurement")]
                    self.runtime.emit_blink_reject(
                        "connect-timeout",
                        BlinkRejectReasonV3::OperationTimeout,
                    );
                    self.runtime.record_a1(A1Outcome::TransportFailure);
                    self.runtime.set_a1_status(A1Status::Retrying);
                    if !self.wait_backoff(backoff).await {
                        return;
                    }
                    backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
                    continue;
                }
            };

            self.runtime.set_a1_status(A1Status::AwaitingAck);
            let acknowledgment = time::timeout(OPERATION_TIMEOUT, async {
                socket.send(Message::Text(BLINK_SUBSCRIBE.into())).await?;
                socket.flush().await?;
                socket.next().await.unwrap_or(Err(WebSocketError::ConnectionClosed))
            })
            .await;
            let subscription = match acknowledgment {
                Ok(Ok(Message::Text(text))) if text.len() <= MAX_WIRE_BYTES => {
                    match Self::subscription_from_ack(text.as_str()) {
                        Some(subscription) => subscription,
                        None => {
                            #[cfg(feature = "edge-measurement")]
                            self.runtime.emit_blink_reject(
                                "ack-malformed",
                                BlinkRejectReasonV3::AckMalformed,
                            );
                            self.runtime.record_a1(A1Outcome::ProtocolDisabled);
                            self.runtime.set_a1_status(A1Status::DisabledPermanent);
                            self.runtime.shutdown().cancel();
                            return;
                        }
                    }
                }
                Ok(Ok(Message::Ping(_) | Message::Pong(_))) => {
                    #[cfg(feature = "edge-measurement")]
                    self.runtime.emit_blink_reject("ack-control", BlinkRejectReasonV3::AckControl);
                    self.runtime.record_a1(A1Outcome::ProtocolDisabled);
                    self.runtime.set_a1_status(A1Status::DisabledPermanent);
                    self.runtime.shutdown().cancel();
                    return;
                }
                Ok(Ok(message)) => {
                    let _message_kind = &message;
                    #[cfg(feature = "edge-measurement")]
                    {
                        let (branch_id, reason) = match message {
                            Message::Text(_) => {
                                ("ack-text-oversize", BlinkRejectReasonV3::AckTextOversize)
                            }
                            Message::Binary(_) => ("ack-binary", BlinkRejectReasonV3::AckBinary),
                            Message::Close(_) => ("ack-close", BlinkRejectReasonV3::AckClose),
                            _ => ("ack-unexpected-wire", BlinkRejectReasonV3::AckUnexpectedWire),
                        };
                        self.runtime.emit_blink_reject(branch_id, reason);
                    }
                    self.runtime.record_a1(A1Outcome::ProtocolDisabled);
                    self.runtime.set_a1_status(A1Status::DisabledPermanent);
                    self.runtime.shutdown().cancel();
                    return;
                }
                Ok(Err(error)) => {
                    let (status, outcome, retry, cancel_root) =
                        self.classify_error_and_emit(&error);
                    if let Some(outcome) = outcome {
                        self.runtime.record_a1(outcome);
                    }
                    self.runtime.set_a1_status(status);
                    if cancel_root {
                        self.runtime.shutdown().cancel();
                    }
                    if !retry || !self.wait_backoff(backoff).await {
                        return;
                    }
                    backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
                    continue;
                }
                Err(_) => {
                    #[cfg(feature = "edge-measurement")]
                    self.runtime
                        .emit_blink_reject("ack-timeout", BlinkRejectReasonV3::OperationTimeout);
                    self.runtime.record_a1(A1Outcome::TransportFailure);
                    self.runtime.set_a1_status(A1Status::Retrying);
                    if !self.wait_backoff(backoff).await {
                        return;
                    }
                    backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
                    continue;
                }
            };

            self.runtime.set_a1_status(A1Status::Subscribed);
            backoff = INITIAL_BACKOFF;
            let retry = loop {
                let message = tokio::select! {
                    () = self.runtime.shutdown().wait_cancelled() => {
                        self.runtime.set_a1_status(A1Status::Closed);
                        return;
                    }
                    message = socket.next() => message,
                };
                match message {
                    Some(Ok(Message::Text(text))) if text.len() <= MAX_WIRE_BYTES => {
                        match BlinkVictim::decode(text.as_str(), &subscription, Instant::now()) {
                            Ok(victim) => {
                                self.runtime.record_a1(A1Outcome::NotificationDecodedSubmitted);
                                self.runtime.submit_blink_victim(victim);
                            }
                            Err(A1Outcome::ApplicationDrop) => {
                                #[cfg(feature = "edge-measurement")]
                                {
                                    let (branch_id, reason) =
                                        BlinkVictim::classify_decode_rejection_branch(
                                            text.as_str(),
                                            &subscription,
                                            A1Outcome::ApplicationDrop,
                                        );
                                    self.runtime.emit_blink_reject(branch_id, reason);
                                }
                                self.runtime.record_a1(A1Outcome::ApplicationDrop);
                            }
                            Err(A1Outcome::ProtocolDisabled) => {
                                #[cfg(feature = "edge-measurement")]
                                {
                                    let (branch_id, reason) =
                                        BlinkVictim::classify_decode_rejection_branch(
                                            text.as_str(),
                                            &subscription,
                                            A1Outcome::ProtocolDisabled,
                                        );
                                    self.runtime.emit_blink_reject(branch_id, reason);
                                }
                                self.runtime.record_a1(A1Outcome::ProtocolDisabled);
                                self.runtime.set_a1_status(A1Status::DisabledPermanent);
                                self.runtime.shutdown().cancel();
                                return;
                            }
                            Err(_) => {
                                #[cfg(feature = "edge-measurement")]
                                self.runtime.emit_blink_reject(
                                    "notification-internal-failure",
                                    BlinkRejectReasonV3::NotificationInternalFailure,
                                );
                                self.runtime.record_a1(A1Outcome::InternalFailure);
                                self.runtime.set_a1_status(A1Status::DisabledPermanent);
                                self.runtime.shutdown().cancel();
                                return;
                            }
                        }
                    }
                    Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) => {
                        let _message_kind = &message;
                        #[cfg(feature = "edge-measurement")]
                        {
                            let (branch_id, reason) = match message {
                                Message::Text(_) => {
                                    ("wire-text-oversize", BlinkRejectReasonV3::WireTextOversize)
                                }
                                _ => ("wire-binary", BlinkRejectReasonV3::WireBinary),
                            };
                            self.runtime.emit_blink_reject(branch_id, reason);
                        }
                        self.runtime.record_a1(A1Outcome::ApplicationDrop);
                    }
                    Some(Ok(message @ (Message::Ping(_) | Message::Pong(_)))) => {
                        let _message_kind = &message;
                        #[cfg(feature = "edge-measurement")]
                        {
                            let (branch_id, reason) = match message {
                                Message::Ping(_) => {
                                    ("wire-ping", BlinkRejectReasonV3::WireControlPing)
                                }
                                _ => ("wire-pong", BlinkRejectReasonV3::WireControlPong),
                            };
                            self.runtime.emit_blink_reject(branch_id, reason);
                        }
                        self.runtime.record_a1(A1Outcome::ControlObserved);
                    }
                    Some(Ok(Message::Close(_))) => {
                        #[cfg(feature = "edge-measurement")]
                        self.runtime
                            .emit_blink_reject("wire-close", BlinkRejectReasonV3::WireClose);
                        self.runtime.record_a1(A1Outcome::DisconnectObserved);
                        break true;
                    }
                    Some(Ok(Message::Frame(_))) => {
                        #[cfg(feature = "edge-measurement")]
                        self.runtime.emit_blink_reject(
                            "wire-frame",
                            BlinkRejectReasonV3::WireUnexpectedFrame,
                        );
                        self.runtime.record_a1(A1Outcome::ProtocolDisabled);
                        self.runtime.set_a1_status(A1Status::DisabledPermanent);
                        self.runtime.shutdown().cancel();
                        return;
                    }
                    Some(Err(error)) => {
                        let (status, outcome, retry, cancel_root) =
                            self.classify_error_and_emit(&error);
                        if let Some(outcome) = outcome {
                            self.runtime.record_a1(outcome);
                        }
                        self.runtime.set_a1_status(status);
                        if cancel_root {
                            self.runtime.shutdown().cancel();
                        }
                        if !retry {
                            return;
                        }
                        break true;
                    }
                    None => {
                        #[cfg(feature = "edge-measurement")]
                        self.runtime.emit_blink_reject("wire-end", BlinkRejectReasonV3::WireEnd);
                        self.runtime.record_a1(A1Outcome::DisconnectObserved);
                        break true;
                    }
                }
            };
            if retry {
                self.runtime.set_a1_status(A1Status::Retrying);
                if !self.wait_backoff(backoff).await {
                    return;
                }
                backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
            }
        }
    }

    async fn wait_backoff(&self, delay: Duration) -> bool {
        tokio::select! {
            () = self.runtime.shutdown().wait_cancelled() => {
                self.runtime.set_a1_status(A1Status::Closed);
                false
            }
            () = time::sleep(delay.min(MAX_BACKOFF)) => true,
        }
    }

    fn subscription_from_ack(text: &str) -> Option<String> {
        #[derive(Deserialize)]
        struct Ack {
            jsonrpc: String,
            id: Value,
            result: Option<String>,
            error: Option<Value>,
        }

        let value: Value = serde_json::from_str(text).ok()?;
        if value.as_object()?.contains_key("error") {
            return None;
        }
        let ack: Ack = serde_json::from_str(text).ok()?;
        let subscription = ack.result?;
        (ack.jsonrpc == "2.0"
            && ack.id.as_u64() == Some(1)
            && ack.error.is_none()
            && !subscription.is_empty())
        .then_some(subscription)
    }

    fn classify_error_and_emit(
        &self,
        error: &WebSocketError,
    ) -> (A1Status, Option<A1Outcome>, bool, bool) {
        #[cfg(feature = "edge-measurement")]
        {
            let disposition = BlinkRejectClassifierV3::classify(error);
            self.emit_classified_reason(disposition.reason);
        }
        Self::classify_error(error)
    }

    #[cfg(feature = "edge-measurement")]
    fn emit_classified_reason(&self, reason: BlinkRejectReasonV3) {
        if let Some(branch_id) = BlinkRejectClassifierV3::branch_id(reason) {
            self.runtime.emit_blink_reject(branch_id, reason);
        }
    }

    fn classify_error(error: &WebSocketError) -> (A1Status, Option<A1Outcome>, bool, bool) {
        match error {
            WebSocketError::ConnectionClosed
            | WebSocketError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => {
                (A1Status::Retrying, Some(A1Outcome::DisconnectObserved), true, false)
            }
            WebSocketError::AlreadyClosed | WebSocketError::WriteBufferFull(_) => {
                (A1Status::DisabledPermanent, Some(A1Outcome::InternalFailure), false, true)
            }
            WebSocketError::Io(error) => match error.kind() {
                std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::NetworkDown
                | std::io::ErrorKind::NetworkUnreachable
                | std::io::ErrorKind::HostUnreachable => {
                    (A1Status::Retrying, Some(A1Outcome::TransportFailure), true, false)
                }
                _ => (A1Status::DisabledPermanent, Some(A1Outcome::InternalFailure), false, true),
            },
            WebSocketError::Tls(_)
            | WebSocketError::Capacity(_)
            | WebSocketError::Protocol(_)
            | WebSocketError::Utf8(_)
            | WebSocketError::AttackAttempt
            | WebSocketError::Url(_)
            | WebSocketError::HttpFormat(_) => {
                (A1Status::DisabledPermanent, Some(A1Outcome::ProtocolDisabled), false, false)
            }
            WebSocketError::Http(response) => match response.status().as_u16() {
                101 => (A1Status::AwaitingAck, None, false, false),
                408 | 429 | 500..=599 => {
                    (A1Status::Retrying, Some(A1Outcome::TransportFailure), true, false)
                }
                _ => (A1Status::DisabledPermanent, Some(A1Outcome::ProtocolDisabled), false, false),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        sync::Arc,
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use tokio_tungstenite::{
        accept_async,
        tungstenite::{
            error::{CapacityError, UrlError},
            http::Response,
        },
    };

    use super::*;

    fn credential_file(name: &str, value: &[u8], mode: u32) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "base-mev-trader-{name}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::write(&path, value).expect("credential fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("fixture mode");
        path
    }

    fn notification(subscription: &str, block: &str, index: &str, raw_tx: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"eth_subscription","params":{{"subscription":"{subscription}","timestamp":1,"publishTime":2,"blockNumber":"{block}","flashblockIndex":"{index}","result":{{"chainId":"0xd","type":"0x2","hash":"0x0000000000000000000000000000000000000000000000000000000000000001","from":"0x0000000000000000000000000000000000000002","rawTx":"{raw_tx}"}}}}}}"#
        )
    }

    #[test]
    fn credential_is_bounded_validated_and_redacted() {
        let valid = credential_file("valid", b"receive-only_1\r\n", 0o600);
        let credential = BlinkCredential::load(valid.clone().into_os_string()).expect("credential");
        assert_eq!(format!("{credential:?}"), "BlinkCredential { value: \"[REDACTED]\" }");

        let permissive = credential_file("permissive", b"secret", 0o640);
        assert!(BlinkCredential::load(permissive.clone().into_os_string()).is_none());
        let newline = credential_file("newline", b"bad\nvalue", 0o600);
        assert!(BlinkCredential::load(newline.clone().into_os_string()).is_none());
        let oversized = credential_file("oversized", &[b'a'; MAX_CREDENTIAL_BYTES + 1], 0o600);
        assert!(BlinkCredential::load(oversized.clone().into_os_string()).is_none());

        let link = valid.with_extension("link");
        symlink(&valid, &link).expect("symlink fixture");
        assert!(BlinkCredential::load(link.clone().into_os_string()).is_none());
        for path in [valid, permissive, newline, oversized, link] {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn authoritative_notification_is_strict_and_bounded() {
        let now = Instant::now();
        let valid = notification("sub", "0x64", "0x2", "0x01");
        let victim = BlinkVictim::decode(&valid, "sub", now).expect("notification");
        assert_eq!(victim.block_number(), 100);
        assert_eq!(victim.flashblock_index(), 2);
        assert_eq!(victim.chain_id(), 13);
        assert_eq!(victim.transaction_type(), 2);
        assert_eq!(victim.raw_tx().as_ref(), &[1]);
        assert_eq!(victim.received_at(), now);

        assert_eq!(BlinkVictim::decode(&valid, "other", now), Err(A1Outcome::ProtocolDisabled));
        assert_eq!(
            BlinkVictim::decode(&notification("sub", "0x00", "0x2", "0x01"), "sub", now),
            Err(A1Outcome::ApplicationDrop)
        );
        assert_eq!(
            BlinkVictim::decode(&notification("sub", "0x64", "0x2", "0x"), "sub", now),
            Err(A1Outcome::ApplicationDrop)
        );
        let too_large = format!("0x{}", "00".repeat(MAX_RAW_TX_BYTES + 1));
        assert_eq!(
            BlinkVictim::decode(&notification("sub", "0x64", "0x2", &too_large), "sub", now,),
            Err(A1Outcome::ApplicationDrop)
        );
    }

    #[test]
    fn ack_and_error_classification_are_closed() {
        assert_eq!(
            BlinkFeedClient::subscription_from_ack(r#"{"jsonrpc":"2.0","id":1,"result":"sub"}"#),
            Some("sub".to_owned())
        );
        for invalid in [
            r#"{"jsonrpc":"2.0","id":"1","result":"sub"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":""}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":"sub","error":null}"#,
            r#"{"jsonrpc":"2.0","id":1,"id":1,"result":"sub"}"#,
        ] {
            assert!(BlinkFeedClient::subscription_from_ack(invalid).is_none());
        }

        let transient = [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::NotConnected,
            std::io::ErrorKind::NetworkDown,
            std::io::ErrorKind::NetworkUnreachable,
            std::io::ErrorKind::HostUnreachable,
        ];
        for kind in transient {
            let policy =
                BlinkFeedClient::classify_error(&WebSocketError::Io(std::io::Error::from(kind)));
            assert_eq!(
                policy,
                (A1Status::Retrying, Some(A1Outcome::TransportFailure), true, false)
            );
        }
        assert_eq!(
            BlinkFeedClient::classify_error(&WebSocketError::Io(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))),
            (A1Status::DisabledPermanent, Some(A1Outcome::InternalFailure), false, true)
        );
        assert_eq!(
            BlinkFeedClient::classify_error(&WebSocketError::ConnectionClosed),
            (A1Status::Retrying, Some(A1Outcome::DisconnectObserved), true, false)
        );
        assert_eq!(
            BlinkFeedClient::classify_error(&WebSocketError::AlreadyClosed),
            (A1Status::DisabledPermanent, Some(A1Outcome::InternalFailure), false, true)
        );

        for error in [
            WebSocketError::Capacity(CapacityError::MessageTooLong { size: 2, max_size: 1 }),
            WebSocketError::Protocol(ProtocolError::WrongHttpMethod),
            WebSocketError::Utf8("invalid".to_owned()),
            WebSocketError::AttackAttempt,
            WebSocketError::Url(UrlError::NoHostName),
        ] {
            let policy = BlinkFeedClient::classify_error(&error);
            assert_eq!(
                policy,
                (A1Status::DisabledPermanent, Some(A1Outcome::ProtocolDisabled), false, false)
            );
        }
        assert_eq!(
            BlinkFeedClient::classify_error(&WebSocketError::Protocol(
                ProtocolError::ResetWithoutClosingHandshake,
            )),
            (A1Status::Retrying, Some(A1Outcome::DisconnectObserved), true, false)
        );
        assert_eq!(
            BlinkFeedClient::classify_error(&WebSocketError::WriteBufferFull(Box::new(
                Message::Text("full".into()),
            ))),
            (A1Status::DisabledPermanent, Some(A1Outcome::InternalFailure), false, true)
        );

        for (status, expected) in [
            (101, (A1Status::AwaitingAck, None, false, false)),
            (408, (A1Status::Retrying, Some(A1Outcome::TransportFailure), true, false)),
            (429, (A1Status::Retrying, Some(A1Outcome::TransportFailure), true, false)),
            (500, (A1Status::Retrying, Some(A1Outcome::TransportFailure), true, false)),
            (599, (A1Status::Retrying, Some(A1Outcome::TransportFailure), true, false)),
            (200, (A1Status::DisabledPermanent, Some(A1Outcome::ProtocolDisabled), false, false)),
            (401, (A1Status::DisabledPermanent, Some(A1Outcome::ProtocolDisabled), false, false)),
        ] {
            let response =
                Response::builder().status(status).body(None).expect("HTTP response fixture");
            assert_eq!(
                BlinkFeedClient::classify_error(&WebSocketError::Http(Box::new(response))),
                expected
            );
        }
    }

    #[test]
    fn shutdown_waiter_and_counters_are_finite() {
        let runtime =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let shutdown = Arc::new(RuntimeShutdown::default());
            let waiter = Arc::clone(&shutdown);
            let task = tokio::spawn(async move { waiter.wait_cancelled().await });
            shutdown.cancel();
            task.await.expect("waiter");
        });

        let counters = A1Counters::default();
        counters.values[A1Outcome::ApplicationDrop.index()].store(u64::MAX, Ordering::Relaxed);
        counters.record(A1Outcome::ApplicationDrop);
        assert_eq!(counters.count(A1Outcome::ApplicationDrop), u64::MAX);
        assert_eq!(counters.snapshot().len(), A1_OUTCOME_COUNT);
    }

    #[test]
    fn loopback_101_sends_one_subscribe_and_decodes_without_egress() {
        let io =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        io.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback");
            let endpoint = format!("ws://{}/ws/v1/", listener.local_addr().expect("address"));
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                let mut socket = accept_async(stream).await.expect("101 handshake");
                let subscribe = socket.next().await.expect("subscribe").expect("text");
                assert_eq!(subscribe, Message::Text(BLINK_SUBSCRIBE.into()));
                socket
                    .send(Message::Text(
                        r#"{"jsonrpc":"2.0","id":1,"result":"loopback-sub"}"#.into(),
                    ))
                    .await
                    .expect("ack");
                socket
                    .send(Message::Text(notification("loopback-sub", "0x64", "0x2", "0x01").into()))
                    .await
                    .expect("notification");
                socket.send(Message::Binary(vec![1].into())).await.expect("binary");
                socket.send(Message::Ping(vec![2].into())).await.expect("ping");
                socket.send(Message::Pong(vec![3].into())).await.expect("pong");
                socket.send(Message::Close(None)).await.expect("close");
            });

            let credential = credential_file("loopback-101", b"synthetic", 0o600);
            let runtime = Arc::new(
                MevTraderRuntime::start(
                    crate::MevTraderRuntimeConfig::empty().expect("empty config"),
                )
                .expect("trader runtime"),
            );
            let client = BlinkFeedClient::new(
                BlinkIngressConfig {
                    credential_file: credential.clone().into_os_string(),
                    endpoint: Some(endpoint),
                },
                Arc::clone(&runtime),
            )
            .expect("client");
            let client_task = tokio::spawn(client.run());
            for _ in 0..100 {
                if runtime.counters().count(A1Outcome::DisconnectObserved) == 1 {
                    break;
                }
                time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(runtime.counters().count(A1Outcome::NotificationDecodedSubmitted), 1);
            assert_eq!(runtime.counters().count(A1Outcome::SlotAccepted), 1);
            assert_eq!(runtime.counters().count(A1Outcome::ApplicationDrop), 1);
            assert_eq!(runtime.counters().count(A1Outcome::ControlObserved), 2);
            assert_eq!(runtime.counters().count(A1Outcome::DisconnectObserved), 1);
            runtime.close();
            client_task.await.expect("client task");
            server.await.expect("server task");
            fs::remove_file(credential).expect("remove credential");
        });
    }

    #[test]
    fn loopback_401_and_tls_failure_disable_permanently() {
        let io =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        io.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback");
            let endpoint = format!("ws://{}/ws/v1/", listener.local_addr().expect("address"));
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request).await.expect("read handshake");
                stream
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write 401");
            });
            let credential = credential_file("loopback-401", b"synthetic", 0o600);
            let runtime = Arc::new(
                MevTraderRuntime::start(
                    crate::MevTraderRuntimeConfig::empty().expect("empty config"),
                )
                .expect("trader runtime"),
            );
            BlinkFeedClient::new(
                BlinkIngressConfig {
                    credential_file: credential.clone().into_os_string(),
                    endpoint: Some(endpoint),
                },
                Arc::clone(&runtime),
            )
            .expect("client")
            .run()
            .await;
            assert_eq!(runtime.a1_status(), A1Status::DisabledPermanent);
            assert_eq!(runtime.counters().count(A1Outcome::ProtocolDisabled), 1);
            server.await.expect("server task");
            fs::remove_file(credential).expect("remove credential");

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("TLS loopback");
            let endpoint = format!("wss://{}/ws/v1/", listener.local_addr().expect("address"));
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.expect("TLS accept");
                drop(stream);
            });
            let credential = credential_file("loopback-tls", b"synthetic", 0o600);
            let runtime = Arc::new(
                MevTraderRuntime::start(
                    crate::MevTraderRuntimeConfig::empty().expect("empty config"),
                )
                .expect("trader runtime"),
            );
            BlinkFeedClient::new(
                BlinkIngressConfig {
                    credential_file: credential.clone().into_os_string(),
                    endpoint: Some(endpoint),
                },
                Arc::clone(&runtime),
            )
            .expect("client")
            .run()
            .await;
            assert_eq!(runtime.a1_status(), A1Status::DisabledPermanent);
            assert_eq!(runtime.counters().count(A1Outcome::ProtocolDisabled), 1);
            server.await.expect("server task");
            fs::remove_file(credential).expect("remove credential");
        });
    }
}
