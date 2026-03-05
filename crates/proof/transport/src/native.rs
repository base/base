use async_trait::async_trait;
use base_proof_primitives::{ProofResult, WitnessBundle};
use tokio::sync::{Mutex, mpsc};

use crate::{TransportError, TransportResult, WitnessTransport};

/// In-process witness transport backed by tokio mpsc channels.
///
/// Created via [`NativeTransport::channel`], which returns a sender/receiver
/// pair. The sender submits witness bundles, and the receiver retrieves proof
/// results produced by the backend.
#[derive(Debug)]
pub struct NativeTransport {
    witness_tx: mpsc::Sender<WitnessBundle>,
    result_rx: Mutex<mpsc::Receiver<ProofResult>>,
}

/// The backend half of a [`NativeTransport`] channel.
///
/// The proof backend receives witness bundles from this handle and sends back
/// proof results.
#[derive(Debug)]
pub struct NativeBackend {
    witness_rx: Mutex<mpsc::Receiver<WitnessBundle>>,
    result_tx: mpsc::Sender<ProofResult>,
}

impl NativeTransport {
    /// Create a connected transport/backend pair with the given channel
    /// capacity.
    pub fn channel(capacity: usize) -> (Self, NativeBackend) {
        let (witness_tx, witness_rx) = mpsc::channel(capacity);
        let (result_tx, result_rx) = mpsc::channel(capacity);

        let transport = Self { witness_tx, result_rx: Mutex::new(result_rx) };

        let backend = NativeBackend { witness_rx: Mutex::new(witness_rx), result_tx };

        (transport, backend)
    }
}

impl NativeBackend {
    /// Receive the next witness bundle from the transport.
    pub async fn recv_witness(&self) -> TransportResult<WitnessBundle> {
        self.witness_rx.lock().await.recv().await.ok_or(TransportError::ChannelClosed)
    }

    /// Send a proof result back to the transport.
    pub async fn send_result(&self, result: ProofResult) -> TransportResult<()> {
        self.result_tx.send(result).await.map_err(|e| TransportError::Send(e.to_string()))
    }
}

#[async_trait]
impl WitnessTransport for NativeTransport {
    async fn send_witness(&self, bundle: WitnessBundle) -> TransportResult<()> {
        self.witness_tx.send(bundle).await.map_err(|e| TransportError::Send(e.to_string()))
    }

    async fn recv_result(&self) -> TransportResult<ProofResult> {
        self.result_rx.lock().await.recv().await.ok_or(TransportError::ChannelClosed)
    }
}
