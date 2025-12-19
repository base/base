# base-reth-relay

Encrypted Transaction Relay for Base.

## Overview

This crate provides functionality for relaying encrypted transactions to the sequencer
via the P2P network. It's designed as a fallback mechanism when the primary sequencer
RPC endpoint is unavailable (e.g., during a CDN outage).

## Features

- **ECIES Encryption**: X25519 key exchange + ChaCha20-Poly1305 authenticated encryption
- **Proof-of-Work**: SHA256-based PoW for spam prevention (~10-25ms on average hardware)
- **On-chain Configuration**: Reads relay parameters from an on-chain contract
- **Key Rotation**: Supports current + previous key for graceful rotation

## Components

### Encryption (`crypto` module)

Implements ECIES (Elliptic Curve Integrated Encryption Scheme):

```rust
use base_reth_relay::crypto;

// Generate a keypair (sequencer side)
let (secret_key, public_key) = crypto::generate_keypair();

// Encrypt a transaction (client side)
let encrypted = crypto::encrypt(&public_key, &tx_bytes)?;

// Decrypt (sequencer side)
let decrypted = crypto::decrypt(&secret_key, &encrypted)?;
```

### Proof-of-Work (`pow` module)

Spam prevention via SHA256-based proof-of-work:

```rust
use base_reth_relay::pow;

// Compute PoW (client side, ~15ms at difficulty 18)
let nonce = pow::compute(&encrypted_payload, difficulty);

// Verify PoW (relay/sequencer side, ~1μs)
pow::verify(&encrypted_payload, nonce, difficulty)?;
```

### Configuration (`config` module)

Reads parameters from the `EncryptedRelayConfig` contract:

```rust
use base_reth_relay::config::{RelayConfigCache, fetch_config};

// Fetch config from chain
let params = fetch_config(&provider, contract_address).await?;

// Create a cache with background polling
let mut cache = RelayConfigCache::new(params);
cache.start_polling(provider, config);

// Check if a key is valid
if cache.is_valid_encryption_key(&pubkey) {
    // Process request...
}
```

## Protocol Flow

1. **Client**: Fetches relay parameters via `base_getRelayParameters`
2. **Client**: Encrypts transaction with sequencer's public key
3. **Client**: Computes proof-of-work over encrypted payload
4. **Client**: Submits via `base_sendEncryptedTransaction` to any Base node
5. **Relay Node**: Verifies PoW, validates encryption key
6. **Relay Node**: Forwards to sequencer via P2P relay protocol
7. **Sequencer**: Decrypts, validates transaction, queues for inclusion

## On-chain Contract

The `EncryptedRelayConfig` contract stores:

- `encryptionPubkey`: Current X25519 public key (32 bytes)
- `previousEncryptionPubkey`: Previous key for grace period
- `attestationPubkey`: Ed25519 key for sequencer node attestations
- `powDifficulty`: Current PoW difficulty (leading zero bits)
- `attestationValiditySeconds`: How long attestations remain valid

## P2P Relay Protocol

The P2P relay system enables non-sequencer nodes to forward encrypted transactions to the sequencer.

### Architecture

```
User -> RPC (base_sendEncryptedTransaction) -> Relay Node
    -> Validates PoW, encryption key
    -> Discovers sequencer via ENR attestation
    -> Forwards via P2P subprotocol
    -> Sequencer receives, decrypts, submits to mempool
```

### Key Management (`keys` module)

Uses a **single Ed25519 keypair** for the sequencer with X25519 derivation:

```rust
use base_reth_relay::SequencerKeypair;

// Generate a new keypair
let keypair = SequencerKeypair::generate();

// Load from file (32-byte Ed25519 secret key)
let keypair = SequencerKeypair::load_from_file(Path::new("/path/to/key"))?;

// Get public keys for on-chain contract
let x25519_pubkey = keypair.x25519_public_key();  // For encryptionPubkey
let ed25519_pubkey = keypair.ed25519_public_key(); // For attestationPubkey

// Get secret key for decryption
let secret = keypair.x25519_secret_bytes();
```

The X25519 key is derived from Ed25519 using SHA-512 hash + clamping (same as libsodium).

### Attestation (`attestation` module)

Sequencer nodes sign attestations to prove their identity:

```rust
use base_reth_relay::{create_attestation, verify_attestation};

// Sequencer creates attestation for its node ID
let attestation = create_attestation(node_id, &keypair);

// Relay nodes verify attestation against on-chain pubkey
verify_attestation(&attestation, &attestation_pubkey)?;
```

Attestation format:
- `node_id`: B256 - The peer's node ID
- `timestamp`: u64 - Unix timestamp
- `signature`: Bytes - Ed25519 signature over `node_id || timestamp`

### P2P Subprotocol (`p2p` module)

#### Wire Messages (RLP-encoded)

| ID | Name | Direction | Fields |
|----|------|-----------|--------|
| 0x00 | EncryptedTx | Relay->Seq | `request_id`, `encrypted_payload`, `pow_nonce`, `encryption_pubkey` |
| 0x01 | Ack | Seq->Relay | `request_id`, `accepted`, `error_code`, `commitment` |

```rust
use base_reth_relay::p2p::{EncryptedTxMessage, AckMessage};

// Create encrypted tx message
let msg = EncryptedTxMessage {
    request_id: 1,
    encrypted_payload: encrypted.into(),
    pow_nonce: nonce,
    encryption_pubkey: pubkey.into(),
};

// Create ack message
let ack = AckMessage::success(request_id, commitment);
let ack = AckMessage::failure(request_id, commitment, error_code);
```

#### Discovery (`p2p::discovery`)

Sequencer nodes include attestations in their ENR:

```rust
use base_reth_relay::p2p::{SequencerPeerTracker, encode_enr_attestation};

// Encode attestation for ENR
let enr_value = encode_enr_attestation(&attestation);
// Set in ENR with key "relay-attest"

// Track sequencer peers
let tracker = SequencerPeerTracker::new(config_cache);
tracker.validate_and_add_peer(peer_id, &attestation)?;
let sequencers = tracker.sequencer_peers();
```

#### Forwarding (`p2p::forwarder`)

Non-sequencer nodes forward validated requests:

```rust
use base_reth_relay::p2p::{RelayForwarder, create_forward_channel, ForwardRequest};

// Create channel for RPC -> Forwarder communication
let (tx, rx) = create_forward_channel(1000);

// Create forwarder task
let forwarder = RelayForwarder::new(rx, sequencer_tracker, peer_sender);
tokio::spawn(forwarder.run());

// RPC layer sends requests through channel
let (response_tx, response_rx) = oneshot::channel();
tx.send(ForwardRequest { request, response_tx }).await?;
let result = response_rx.await?;
```

#### Sequencer Processing (`p2p::sequencer`)

Sequencer nodes decrypt and submit transactions:

```rust
use base_reth_relay::p2p::{SequencerProcessor, create_receive_channel};

// Create channel for P2P -> Processor communication
let (tx, rx) = create_receive_channel(1000);

// Create processor with decryption key
let processor = SequencerProcessor::new(
    rx,
    keypair.x25519_secret_bytes(),
    config_cache,
    tx_submitter,
);
tokio::spawn(processor.run());
```

## CLI Usage

### Relay Node (non-sequencer)

```bash
./base-reth-node \
    --enable-relay \
    --relay-config-rpc-url https://l1-rpc.example.com \
    --relay-config-contract 0x...
```

### Sequencer Node

```bash
./base-reth-node \
    --enable-relay \
    --relay-sequencer \
    --relay-keypair /path/to/ed25519.key \
    --relay-config-rpc-url https://l1-rpc.example.com \
    --relay-config-contract 0x...
```

### Devnet (generate keypair)

```bash
./base-reth-node \
    --enable-relay \
    --relay-sequencer \
    --relay-keypair-generate
```

## CLI Flags Reference

| Flag | Env Var | Description |
|------|---------|-------------|
| `--enable-relay` | - | Enable encrypted relay RPC endpoints |
| `--relay-sequencer` | `RELAY_SEQUENCER` | Run as sequencer (decrypt locally) |
| `--relay-keypair <PATH>` | `RELAY_KEYPAIR` | Path to Ed25519 keypair file |
| `--relay-keypair-hex <HEX>` | `RELAY_KEYPAIR_HEX` | Hex-encoded Ed25519 keypair (devnet) |
| `--relay-keypair-generate` | `RELAY_KEYPAIR_GENERATE` | Generate keypair on startup (devnet) |
| `--relay-config-rpc-url <URL>` | `RELAY_CONFIG_RPC_URL` | L1 RPC URL for config fetching |
| `--relay-config-contract <ADDR>` | `RELAY_CONFIG_CONTRACT` | EncryptedRelayConfig contract address |
| `--relay-config-poll-interval <SECS>` | `RELAY_CONFIG_POLL_INTERVAL` | Config poll interval (default: 60) |
| `--relay-encryption-pubkey <HEX>` | - | Override encryption pubkey (testing) |
| `--relay-attestation-pubkey <HEX>` | - | Override attestation pubkey (testing) |
| `--relay-pow-difficulty <N>` | - | Override PoW difficulty (default: 18) |

## Module Structure

```
crates/relay/src/
├── lib.rs              # Crate root, re-exports
├── attestation.rs      # Ed25519 attestation signing/verification
├── config.rs           # On-chain config reader with polling
├── crypto.rs           # ECIES encryption (X25519 + ChaCha20-Poly1305)
├── error.rs            # Error types
├── keys.rs             # Ed25519/X25519 keypair management
├── pow.rs              # SHA256-based proof-of-work
├── types.rs            # Request/response types
└── p2p/
    ├── mod.rs          # P2P module root
    ├── discovery.rs    # ENR-based sequencer discovery
    ├── forwarder.rs    # Non-sequencer forwarding task
    ├── messages.rs     # RLP wire protocol messages
    └── sequencer.rs    # Sequencer processing task
```

## Security Considerations

- Encrypted payloads cannot be validated by relay nodes, only forwarded
- PoW prevents spam but doesn't guarantee transaction validity
- Key rotation should use 24h grace period for in-flight transactions
- Attestations should be short-lived (24h) to limit revocation window
- X25519 derivation from Ed25519 uses standard clamping for security
- Sequencer re-validates PoW on receipt (defense in depth)
