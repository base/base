//! In-process gossip transport for action tests.
//!
//! Provides a channel-backed [`GossipTransport`] implementation that routes
//! blocks between test actors without opening real network ports. When a
//! signing key and expected signer address are configured, the transport
//! enforces the same signature validation as production [`BlockHandler`] code.

use alloy_primitives::{Address, B256, Signature, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use base_common_rpc_types_engine::{
    BaseExecutionPayloadEnvelope, NetworkPayloadEnvelope, PayloadHash,
};
use base_consensus_gossip::P2pRpcRequest;
use base_consensus_node::GossipTransport;
use tokio::sync::mpsc;

/// Handle for injecting blocks into a [`TestGossipTransport`].
///
/// Held by test code or the sequencer. Call [`send`] to deliver a
/// [`NetworkPayloadEnvelope`] to the matching [`TestGossipTransport`].
///
/// [`send`]: SupervisedP2P::send
#[derive(Debug, Clone)]
pub struct SupervisedP2P {
    tx: mpsc::UnboundedSender<NetworkPayloadEnvelope>,
}

impl SupervisedP2P {
    /// Send a [`NetworkPayloadEnvelope`] into the transport channel.
    pub fn send(&self, payload: NetworkPayloadEnvelope) {
        let _ = self.tx.send(payload);
    }
}

/// Channel-backed [`GossipTransport`] for action tests.
///
/// Routes blocks between test actors in-process without touching the network.
/// Use [`channel`] to construct the [`SupervisedP2P`] / [`TestGossipTransport`]
/// pair.
///
/// ### Signature validation
///
/// By default, all received blocks are forwarded regardless of signature. Call
/// [`set_block_signer`] to configure the expected signer address and
/// [`set_chain_id`] to supply the chain ID. Once both are set, every block
/// delivered via [`try_next_unsafe_block`] or [`next_unsafe_block`] is checked
/// against the production signing formula:
///
/// ```text
/// msg  = keccak256(domain || chain_id_padded || keccak256(SSZ(payload)))
/// signer = ecrecover(envelope.signature, msg)
/// valid  = signer == expected_signer
/// ```
///
/// Invalid blocks are silently discarded so the node never sees them, exactly
/// as in production where [`BlockHandler`] rejects invalid gossip before
/// forwarding to the derivation pipeline.
///
/// ### `publish()` and signing
///
/// [`GossipTransport::publish`] is used in self-loop scenarios where the same
/// transport both publishes and receives. If a `signing_key` is set via
/// [`set_signing_key`], `publish` signs the outbound envelope with the
/// production formula so that the validation check in [`next_unsafe_block`]
/// passes. Without a signing key, `publish` emits a zero-signature envelope;
/// calling `publish` on a transport where signature validation is active but no
/// signing key is set will panic rather than silently hang the receiver.
///
/// Use [`ActionTestHarness::create_signing_p2p`] to configure both the signing
/// key and the expected signer address together.
///
/// [`channel`]: TestGossipTransport::channel
/// [`set_block_signer`]: GossipTransport::set_block_signer
/// [`set_chain_id`]: TestGossipTransport::set_chain_id
/// [`set_signing_key`]: TestGossipTransport::set_signing_key
/// [`try_next_unsafe_block`]: TestGossipTransport::try_next_unsafe_block
/// [`next_unsafe_block`]: GossipTransport::next_unsafe_block
/// [`BlockHandler`]: base_consensus_gossip::BlockHandler
/// [`ActionTestHarness::create_signing_p2p`]: crate::ActionTestHarness::create_signing_p2p
#[derive(Debug)]
pub struct TestGossipTransport {
    tx: mpsc::UnboundedSender<NetworkPayloadEnvelope>,
    rx: mpsc::UnboundedReceiver<NetworkPayloadEnvelope>,
    /// Expected signer address, set via [`GossipTransport::set_block_signer`].
    expected_signer: Option<Address>,
    /// Chain ID used in the signature message, set via [`set_chain_id`].
    ///
    /// [`set_chain_id`]: TestGossipTransport::set_chain_id
    chain_id: Option<u64>,
    /// Signing key used by [`GossipTransport::publish`] to sign outbound
    /// envelopes. Required when signature validation is active; set via
    /// [`set_signing_key`].
    ///
    /// [`set_signing_key`]: TestGossipTransport::set_signing_key
    signing_key: Option<PrivateKeySigner>,
}

impl TestGossipTransport {
    /// Create a [`SupervisedP2P`] / [`TestGossipTransport`] pair sharing a
    /// single channel, with no signature validation enabled.
    pub fn channel() -> (SupervisedP2P, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            SupervisedP2P { tx: tx.clone() },
            Self { tx, rx, expected_signer: None, chain_id: None, signing_key: None },
        )
    }

    /// Set the chain ID used to construct the signature verification message.
    ///
    /// Must be set alongside [`GossipTransport::set_block_signer`] for
    /// signature validation to activate.
    pub const fn set_chain_id(&mut self, chain_id: u64) {
        self.chain_id = Some(chain_id);
    }

    /// Set the signing key used by [`GossipTransport::publish`].
    ///
    /// Required when signature validation is active and `publish` will be
    /// called (i.e. self-loop test scenarios). Without this, `publish` panics
    /// if `expected_signer` and `chain_id` are configured.
    pub fn set_signing_key(&mut self, key: PrivateKeySigner) {
        self.signing_key = Some(key);
    }

    /// Returns `true` if the envelope carries a valid signature from
    /// [`expected_signer`], or if signature validation is not configured.
    ///
    /// Signature validation is only active when both `expected_signer` and
    /// `chain_id` are set.
    ///
    /// [`expected_signer`]: TestGossipTransport::expected_signer
    fn signature_valid(&self, envelope: &NetworkPayloadEnvelope) -> bool {
        let (Some(signer), Some(chain_id)) = (self.expected_signer, self.chain_id) else {
            return true;
        };
        let msg = envelope.payload_hash.signature_message(chain_id);
        envelope.signature.recover_address_from_prehash(&msg).is_ok_and(|s| s == signer)
    }

    /// Try to receive the next unsafe block without blocking.
    ///
    /// Returns `None` immediately if no block is currently available.
    /// Blocks that fail signature validation (when configured) are silently
    /// discarded and the next available block is returned.
    pub fn try_next_unsafe_block(&mut self) -> Option<NetworkPayloadEnvelope> {
        loop {
            match self.rx.try_recv() {
                Ok(envelope) if self.signature_valid(&envelope) => return Some(envelope),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }
}

/// Infallible error type for [`TestGossipTransport`].
#[derive(Debug)]
pub enum TestGossipTransportError {}

#[async_trait]
impl GossipTransport for TestGossipTransport {
    type Error = TestGossipTransportError;

    async fn publish(&mut self, payload: BaseExecutionPayloadEnvelope) -> Result<(), Self::Error> {
        let parent_beacon_block_root = payload.parent_beacon_block_root;
        let (signature, payload_hash) = if self.expected_signer.is_some() {
            let key = self.signing_key.as_ref().expect(
                "TestGossipTransport: publish() called with signature validation active but \
                 no signing key set — call set_signing_key() or use create_signing_p2p()",
            );
            let chain_id = self.chain_id.expect(
                "TestGossipTransport: expected_signer set but chain_id is not — call set_chain_id()",
            );
            let ph = payload.payload_hash();
            let sig =
                key.sign_hash_sync(&ph.signature_message(chain_id)).expect("signing must not fail");
            (sig, ph)
        } else {
            (Signature::new(U256::ZERO, U256::ZERO, false), PayloadHash(B256::ZERO))
        };
        let _ = self.tx.send(NetworkPayloadEnvelope {
            payload: payload.execution_payload,
            signature,
            payload_hash,
            parent_beacon_block_root,
        });
        Ok(())
    }

    async fn next_unsafe_block(&mut self) -> Option<NetworkPayloadEnvelope> {
        loop {
            let envelope = self.rx.recv().await?;
            if self.signature_valid(&envelope) {
                return Some(envelope);
            }
        }
    }

    fn set_block_signer(&mut self, address: Address) {
        self.expected_signer = Some(address);
    }

    fn handle_p2p_rpc(&mut self, _request: P2pRpcRequest) {}
}
