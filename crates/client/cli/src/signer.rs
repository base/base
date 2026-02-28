//! Signer CLI Flags for consensus clients.
//!
//! This module defines argument types for configuring block signing,
//! supporting both local private keys and remote signers.

use std::{path::PathBuf, str::FromStr};

use alloy_primitives::{Address, B256};
use alloy_signer::{Signer, k256::ecdsa};
use alloy_signer_local::PrivateKeySigner;
use base_consensus_peers::SecretKeyLoader;
use base_consensus_sources::{BlockSigner, ClientCert, RemoteSigner};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use url::Url;

/// Signer CLI Flags
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignerArgs {
    /// An optional flag to specify a local private key for the sequencer to sign unsafe blocks.
    pub sequencer_key: Option<B256>,
    /// An optional path to a file containing the sequencer private key.
    /// This is mutually exclusive with `sequencer_key`.
    pub sequencer_key_path: Option<PathBuf>,
    /// The URL of the remote signer endpoint. If not provided, remote signer will be disabled.
    /// This is mutually exclusive with `sequencer_key`.
    /// This is required if any of the other signer flags are provided.
    pub endpoint: Option<Url>,
    /// The address to sign transactions for. Required if `endpoint` is provided.
    pub address: Option<Address>,
    /// Headers to pass to the remote signer. Format `key=value`. Value can contain any character
    /// allowed in a HTTP header. When using env vars, split with commas. When using flags one
    /// key value pair per flag.
    pub header: Vec<String>,
    /// An optional path to CA certificates to be used for the remote signer.
    pub ca_cert: Option<PathBuf>,
    /// An optional path to the client certificate for the remote signer. If specified,
    /// `key` must also be specified.
    pub cert: Option<PathBuf>,
    /// An optional path to the client key for the remote signer. If specified,
    /// `cert` must also be specified.
    pub key: Option<PathBuf>,
}

/// Errors that can occur when parsing the signer arguments.
#[derive(Debug, thiserror::Error)]
pub enum SignerArgsParseError {
    /// The local sequencer key and remote signer cannot be specified at the same time.
    #[error("local sequencer key and remote signer cannot be specified at the same time")]
    LocalAndRemoteSigner,
    /// Both sequencer key and sequencer key path cannot be specified at the same time.
    #[error(
        "sequencer key and sequencer key path cannot both be specified, use either --p2p.sequencer.key or --p2p.sequencer.key.path"
    )]
    ConflictingSequencerKeyInputs,
    /// The sequencer key is invalid.
    #[error("sequencer key is invalid")]
    SequencerKeyInvalid(#[from] ecdsa::Error),
    /// Failed to load sequencer key from file.
    #[error("failed to load sequencer key from file")]
    SequencerKeyFileError(#[from] base_consensus_peers::KeypairError),
    /// The address is required if `signer.endpoint` is provided.
    #[error("address is required when `signer.endpoint` is provided")]
    AddressRequired,
    /// The header is invalid.
    #[error("header is invalid")]
    InvalidHeader,
    /// The private key field is required if `signer.tls.cert` is provided.
    #[error("private key field is required when `signer.tls.cert` is provided")]
    KeyRequired,
    /// The header name is invalid.
    #[error("header name is invalid")]
    InvalidHeaderName(#[from] reqwest::header::InvalidHeaderName),
    /// The header value is invalid.
    #[error("header value is invalid")]
    InvalidHeaderValue(#[from] reqwest::header::InvalidHeaderValue),
}

impl SignerArgs {
    /// Creates a [`BlockSigner`] from the [`SignerArgs`].
    ///
    /// The `l2_chain_id` is used to set the chain ID on the signer.
    pub fn config(self, l2_chain_id: u64) -> Result<Option<BlockSigner>, SignerArgsParseError> {
        // First, resolve the sequencer key from either raw input or file
        let sequencer_key = self.resolve_sequencer_key()?;

        // The sequencer signer obtained from the CLI arguments.
        let gossip_signer: Option<BlockSigner> = match (sequencer_key, self.config_remote()?) {
            (Some(_), Some(_)) => return Err(SignerArgsParseError::LocalAndRemoteSigner),
            (Some(key), None) => {
                let signer: BlockSigner =
                    PrivateKeySigner::from_bytes(&key)?.with_chain_id(Some(l2_chain_id)).into();
                Some(signer)
            }
            (None, Some(signer)) => Some(signer.into()),
            (None, None) => None,
        };

        Ok(gossip_signer)
    }

    /// Resolves the sequencer key from either the raw key or the key file.
    fn resolve_sequencer_key(&self) -> Result<Option<B256>, SignerArgsParseError> {
        match (self.sequencer_key, &self.sequencer_key_path) {
            (Some(key), None) => Ok(Some(key)),
            (None, Some(path)) => {
                let keypair = SecretKeyLoader::load(path)?;
                // Extract the private key bytes from the secp256k1 keypair
                keypair.try_into_secp256k1().map_or_else(
                    |_| Err(SignerArgsParseError::SequencerKeyInvalid(ecdsa::Error::new())),
                    |secp256k1_keypair| {
                        let private_key_bytes = secp256k1_keypair.secret().to_bytes();
                        let key = B256::from_slice(&private_key_bytes);
                        Ok(Some(key))
                    },
                )
            }
            (Some(_), Some(_)) => Err(SignerArgsParseError::ConflictingSequencerKeyInputs),
            (None, None) => Ok(None),
        }
    }

    /// Creates a [`RemoteSigner`] from the [`SignerArgs`].
    fn config_remote(self) -> Result<Option<RemoteSigner>, SignerArgsParseError> {
        let Some(endpoint) = self.endpoint else {
            return Ok(None);
        };

        let Some(address) = self.address else {
            return Err(SignerArgsParseError::AddressRequired);
        };

        let headers = self
            .header
            .iter()
            .map(|h| {
                let (key, value) = h.split_once('=').ok_or(SignerArgsParseError::InvalidHeader)?;
                Ok((HeaderName::from_str(key)?, HeaderValue::from_str(value)?))
            })
            .collect::<Result<HeaderMap, SignerArgsParseError>>()?;

        let client_cert = self
            .cert
            .clone()
            .map(|cert| {
                Ok::<_, SignerArgsParseError>(ClientCert {
                    cert,
                    key: self.key.clone().ok_or(SignerArgsParseError::KeyRequired)?,
                })
            })
            .transpose()?;

        Ok(Some(RemoteSigner {
            address,
            endpoint,
            ca_cert: self.ca_cert.clone(),
            client_cert,
            headers,
        }))
    }
}
