//! Shared typed errors for basectl command validation and preflight checks.

use std::time::Duration;

use alloy_primitives::{Address, B256};
use alloy_signer_local::LocalSignerError;
use alloy_transport::TransportError;
use alloy_transport_http::reqwest;
use base_proof_contracts::ContractError;
use base_proof_submission::ProofSubmissionError;
use base_prover_service_client::{ProverServiceClientBuildError, ProverServiceClientError};
use base_tx_manager::TxManagerError;
use jsonrpsee::core::client::Error as JsonRpcClientError;
use thiserror::Error;

/// Error returned when a CLI block reference cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum BlockRefParseError {
    /// The provided block reference was empty after trimming whitespace.
    #[error("invalid block reference: empty input")]
    Empty,
    /// A 32-byte hash-shaped block reference could not be parsed as a hash.
    #[error("invalid block reference: malformed hash")]
    MalformedHash {
        /// The original block reference supplied by the caller.
        raw: String,
    },
    /// The block reference was not a supported number, hash, or tag.
    #[error("invalid block reference: {message}")]
    InvalidTag {
        /// The original block reference supplied by the caller.
        raw: String,
        /// The parser error returned by the underlying tag parser.
        message: String,
    },
    /// The `pending` tag is rejected because typed block responses cannot deserialize it.
    #[error("the `pending` tag is not supported; use `latest`, `safe`, `finalized`, or `earliest`")]
    PendingUnsupported,
}

/// Error returned when shared conductor source or node lookup fails.
///
/// Messages are deliberately subject-less fragments; callers wrap this in
/// [`ConductorCommandError`] or [`SequencerCommandError`], which prepend the
/// command group so operators see which one failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum NodeLookupError {
    /// The command could not resolve a conductor source from config or flags.
    #[error(
        "commands need conductor config or a bootstrap RPC URL for '{config_name}'. Set `conductors` or `discovery.bootstrap_rpc` in config, or pass `--conductor-rpc <url>`."
    )]
    MissingSource {
        /// The config name selected for the command.
        config_name: String,
    },
    /// The requested conductor node name was not found.
    #[error("node {requested_node} not found. Available nodes: {}", available_nodes.join(", "))]
    MissingNode {
        /// The node name requested by the caller.
        requested_node: String,
        /// The node names available to the command.
        available_nodes: Vec<String>,
    },
}

/// Error returned when a P2P command target is malformed or unsupported.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum P2pTargetError {
    /// The peer target was empty after trimming whitespace.
    #[error("peer target cannot be empty")]
    EmptyTarget,
    /// A multiaddr target did not include a `/p2p/<peer-id>` component.
    #[error("multiaddr target must include a `/p2p/<peer-id>` component")]
    MultiaddrMissingPeerId {
        /// The target supplied by the caller.
        target: String,
    },
    /// A peer target could not be parsed as an enode or ENR.
    #[error("parsing peer target `{target}` as enode or ENR: {message}")]
    InvalidBootnode {
        /// The target supplied by the caller.
        target: String,
        /// The parser error returned by the underlying bootnode parser.
        message: String,
    },
    /// An ENR target did not contain enough data to derive a libp2p multiaddr.
    #[error(
        "ENR target `{target}` does not include enough information to derive a libp2p multiaddr"
    )]
    EnrMissingMultiaddr {
        /// The target supplied by the caller.
        target: String,
    },
    /// Peer removal, ban, and unban actions do not accept ENR targets.
    #[error(
        "peer removal, ban, and unban actions need a bare libp2p peer ID for CL targets; ENR records are only accepted by add-peer"
    )]
    PeerActionEnrTarget {
        /// The target supplied by the caller.
        target: String,
    },
    /// The peer target contained whitespace.
    #[error("peer target must not contain whitespace")]
    TargetContainsWhitespace {
        /// The target supplied by the caller.
        target: String,
    },
    /// A CL peer-action target was URL-like or multiaddr-like instead of a bare peer ID.
    #[error(
        "peer removal, ban, and unban actions need a bare libp2p peer ID for CL targets, not a URL or multiaddr"
    )]
    PeerActionClTargetNotBarePeerId {
        /// The target supplied by the caller.
        target: String,
    },
    /// A reachability target was not an `enode://` URL, `enr:` record, or
    /// public `IPv4` `/ip4/.../tcp/.../p2p/<peer-id>` multiaddr.
    #[error(
        "reachability target `{target}` must be an execution-layer `enode://` URL, a consensus-layer `enr:` record, or a public-IPv4 `/ip4/.../tcp/.../p2p/<peer-id>` multiaddr"
    )]
    ReachabilityTargetUnsupported {
        /// The target supplied by the caller.
        target: String,
    },
    /// The CL peer ID was too short to plausibly be a libp2p peer ID.
    #[error(
        "CL peer ID `{target}` looks too short to be a valid libp2p peer ID; expected a base58-encoded string (e.g. 16Uiu2HAm...)"
    )]
    ClPeerIdTooShort {
        /// The target supplied by the caller.
        target: String,
        /// The minimum accepted length for a libp2p peer ID.
        min_len: usize,
    },
}

/// Error returned by the `p2p` command group.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum P2pCommandError {
    /// The EL ban target is a trusted peer, which the node silently refuses to ban.
    #[error("cannot ban peer {target}: it is in the trusted set")]
    TrustedElPeerBan {
        /// The enode target supplied by the caller.
        target: String,
    },
}

/// Error returned by the `proofs` command group.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProofsCommandError {
    /// The command could not resolve a prover-service RPC URL from flags or config.
    #[error(
        "proofs commands need a prover-service RPC URL.\n\
         The '{config_name}' config does not set `prover_rpc`.\n\
         Override with `--prover-rpc <url>`, set `BASECTL_PROVER_RPC`, \
         or set `prover_rpc` in your YAML config."
    )]
    MissingProverRpc {
        /// The config name selected for the command.
        config_name: String,
    },
    /// The prover-service HTTP client could not be built.
    #[error("failed to build prover-service client for {endpoint}")]
    BuildClient {
        /// The prover-service RPC URL selected for the command.
        endpoint: String,
        /// The underlying client construction error.
        #[source]
        source: ProverServiceClientBuildError,
    },
    /// A prover-service JSON-RPC request failed.
    #[error("prover-service request `{method}` failed against {endpoint}")]
    Rpc {
        /// The prover-service RPC URL selected for the command.
        endpoint: String,
        /// The prover-service JSON-RPC method that failed.
        method: &'static str,
        /// The underlying client error.
        #[source]
        source: ProverServiceClientError,
    },
    /// The proof did not reach a terminal status within the wait window.
    #[error(
        "proof session {session_id} did not complete within {waited:?}; \
         last observed status: {last_status}; re-run the same command to resume this session"
    )]
    WaitTimeout {
        /// The proof session identifier being waited on.
        session_id: String,
        /// The time spent waiting before giving up.
        waited: Duration,
        /// The last proof status observed before the timeout.
        last_status: String,
    },
    /// The command could not resolve a `DisputeGameFactory` address from flags or config.
    #[error(
        "proofs games needs a DisputeGameFactory address.\n\
         The '{config_name}' config does not set `proofs.dispute_game_factory`.\n\
         Override with `--factory <address>` or set `proofs.dispute_game_factory` \
         in your YAML config."
    )]
    MissingDisputeGameFactory {
        /// The config name selected for the command.
        config_name: String,
    },
    /// The dry-run backend cannot produce proof bytes for `proofs finalize`.
    #[error(
        "`--zk-backend dry-run` produces no submittable proof bytes and cannot finalize; \
         use `basectl proofs propose --zk-backend dry-run` for sizing instead"
    )]
    DryRunCannotFinalize,
    /// An L1 dispute-game contract read failed.
    #[error("dispute game read failed against {endpoint}")]
    L1Contract {
        /// The L1 RPC endpoint origin, with credentials, path, and query
        /// redacted so embedded API keys cannot leak into logs.
        endpoint: String,
        /// The underlying contract client error.
        #[source]
        source: ContractError,
    },
    /// The dispute game cannot accept a ZK proposal proof.
    #[error("game {game} cannot be proven: {reason}")]
    GameNotProvable {
        /// The dispute game proxy address.
        game: String,
        /// Why the game cannot accept a ZK proposal proof.
        reason: String,
    },
    /// A dispute game could not be resolved from an L1 transaction hash.
    #[error("cannot resolve a dispute game from transaction {tx_hash}: {reason}")]
    GameFromTransaction {
        /// The L1 transaction hash supplied by the caller.
        tx_hash: B256,
        /// Why the transaction does not resolve to a created game.
        reason: String,
    },
    /// The dispute game is not registered with the configured factory.
    #[error(
        "game {game} is not registered with the configured DisputeGameFactory at {factory}; \
         check that --factory or proofs.dispute_game_factory matches the factory that created \
         the game"
    )]
    GameNotFromFactory {
        /// The dispute game proxy address supplied by the caller.
        game: Address,
        /// The configured `DisputeGameFactory` address.
        factory: Address,
    },
    /// A proof command would requeue a failed session without explicit approval.
    #[error(
        "proof session {session_id} already exists and failed: {message}. \
         Re-running requeues the proof request; pass --retry-failed to explicitly confirm \
         the retry"
    )]
    FailedSessionRetry {
        /// The failed proof session identifier.
        session_id: String,
        /// The prover-service failure message.
        message: String,
    },
    /// The submitter private key could not be parsed.
    #[error("invalid submitter private key")]
    InvalidSubmitterKey {
        /// The parser error returned by the signer.
        #[source]
        source: LocalSignerError,
    },
    /// No submitter private key was found in the key file or environment.
    #[error(
        "no submitter private key found. Set `BASECTL_SUBMITTER_PRIVATE_KEY` \
         or pass `--private-key-file <path>`."
    )]
    MissingSubmitterKey,
    /// The submitter key file could not be read.
    #[error("reading submitter key file {path}")]
    ReadSubmitterKeyFile {
        /// The key file path supplied by the caller.
        path: String,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },
    /// The proof session has not reached a terminal status yet.
    #[error(
        "proof session {session_id} is not complete yet (status: {status}); \
         re-run with `--wait` or try again later"
    )]
    ProofNotReady {
        /// The proof session identifier.
        session_id: String,
        /// The last observed proof status.
        status: String,
    },
    /// The proof session failed on the prover service.
    #[error("proof session {session_id} failed: {message}")]
    ProofFailed {
        /// The proof session identifier.
        session_id: String,
        /// The prover-service error message.
        message: String,
    },
    /// The completed proof session did not include a result payload.
    #[error("proof session {session_id} succeeded but returned no result payload")]
    ProofResultMissing {
        /// The proof session identifier.
        session_id: String,
    },
    /// The session's result is not a PLONK proposal proof.
    #[error(
        "proof session {session_id} does not hold a snark_plonk proposal proof; \
         request one with `basectl proofs propose`"
    )]
    NotAProposalProof {
        /// The proof session identifier.
        session_id: String,
    },
    /// The PLONK result could not be decoded into an on-chain SP1 proof.
    #[error("proof session {session_id} returned an invalid SP1 PLONK receipt: {message}")]
    InvalidProposalProof {
        /// The proof session identifier.
        session_id: String,
        /// The receipt decoding error.
        message: String,
    },
    /// The L1 transaction manager could not be constructed.
    #[error("failed to build L1 transaction manager for {endpoint}")]
    BuildTxManager {
        /// The L1 RPC endpoint origin, with credentials, path, and query
        /// redacted so embedded API keys cannot leak into logs.
        endpoint: String,
        /// The underlying transaction manager construction error.
        #[source]
        source: TxManagerError,
    },
    /// The `verifyProposalProof` submission failed.
    #[error("verifyProposalProof submission to game {game} failed")]
    Submission {
        /// The dispute game proxy address.
        game: String,
        /// The underlying proof submission error.
        #[source]
        source: ProofSubmissionError,
    },
}

/// Error returned when a command cannot resolve a consensus-node RPC URL from flags or config.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
#[error(
    "{command_name} needs a consensus-node RPC URL.\n\
     The '{config_name}' config does not set `consensus_node_rpc`.\n\
     Override with `--cl-rpc <url>` or set `consensus_node_rpc` in your YAML config."
)]
pub struct MissingConsensusRpcError {
    /// The command that needed a consensus RPC URL.
    pub command_name: &'static str,
    /// The config name selected for the command.
    pub config_name: String,
}

/// Error returned by txpool RPC helpers and command execution.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TxpoolCommandError {
    /// The txpool HTTP client could not be built.
    #[error("failed to build txpool HTTP client for {rpc}")]
    BuildHttpClient {
        /// The execution-layer RPC URL selected for the command.
        rpc: String,
        /// The underlying client construction error.
        #[source]
        source: reqwest::Error,
    },
    /// The txpool admin client could not be built.
    #[error("failed to build txpool admin client for {rpc}")]
    BuildAdminClient {
        /// The execution-layer RPC URL selected for the command.
        rpc: String,
        /// The underlying client construction error.
        #[source]
        source: JsonRpcClientError,
    },
    /// The selected RPC does not expose the requested txpool namespace method.
    #[error("txpool RPC method `{method}` is not exposed by {rpc}")]
    TxpoolMethodUnavailable {
        /// The execution-layer RPC URL selected for the command.
        rpc: String,
        /// The unavailable txpool RPC method.
        method: &'static str,
    },
    /// The selected RPC does not expose the requested admin namespace method.
    #[error("admin RPC method `{method}` is not exposed by {rpc}")]
    AdminMethodUnavailable {
        /// The execution-layer RPC URL selected for the command.
        rpc: String,
        /// The unavailable admin RPC method.
        method: &'static str,
    },
    /// A txpool namespace RPC call failed for a reason other than method availability.
    #[error("txpool RPC method `{method}` failed on {rpc}")]
    TxpoolRpc {
        /// The execution-layer RPC URL selected for the command.
        rpc: String,
        /// The txpool RPC method that failed.
        method: &'static str,
        /// The underlying RPC error.
        #[source]
        source: TransportError,
    },
    /// An admin namespace RPC call failed for a reason other than method availability.
    #[error("admin RPC method `{method}` failed on {rpc}")]
    AdminRpc {
        /// The execution-layer RPC URL selected for the command.
        rpc: String,
        /// The admin RPC method that failed.
        method: &'static str,
        /// The underlying RPC error.
        #[source]
        source: JsonRpcClientError,
    },
}

/// Error returned by the `conductor` command group.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ConductorCommandError {
    /// The shared conductor source or node lookup failed.
    #[error("conductor {0}")]
    NodeLookup(#[from] NodeLookupError),
}

/// Error returned by sequencer command validation and preflight checks.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SequencerCommandError {
    /// The shared conductor source or node lookup failed.
    #[error("sequencer {0}")]
    NodeLookup(#[from] NodeLookupError),
    /// The command could not infer an unsafe head hash from the target node.
    #[error(
        "could not determine unsafe head for {node}; pass an explicit 32-byte hash or restore CL reachability"
    )]
    MissingUnsafeHead {
        /// The target node name.
        node: String,
    },
    /// The target sequencer is already active.
    #[error("sequencer already active on {node}; stop it before starting again")]
    AlreadyActive {
        /// The target node name.
        node: String,
    },
    /// The target sequencer is already stopped.
    #[error("sequencer already stopped on {node}")]
    AlreadyStopped {
        /// The target node name.
        node: String,
    },
    /// The command targeted a node that is known not to be the conductor leader.
    #[error(
        "Node is not the conductor leader. Current leader: {current_leader}. `basectl sequencer {action}` must target the leader instead of {requested_node}."
    )]
    NotCurrentLeader {
        /// The node name requested by the caller.
        requested_node: String,
        /// The node currently observed as conductor leader.
        current_leader: String,
        /// The sequencer action being validated.
        action: &'static str,
    },
    /// The command targeted a follower while no current leader name was available.
    #[error(
        "Node is not the conductor leader. `basectl sequencer {action}` must target the current leader instead of {requested_node}."
    )]
    NotLeader {
        /// The node name requested by the caller.
        requested_node: String,
        /// The sequencer action being validated.
        action: &'static str,
    },
    /// The observed unsafe head is zero, so no safe prestate exists for start.
    #[error("no prestate: engine unsafe head is uninitialized, cannot safely start sequencer")]
    UninitializedUnsafeHead,
    /// The requested unsafe head did not match the node's observed unsafe head.
    #[error(
        "block hash mismatch: engine unsafe head is {observed_hash}, caller requested {requested_hash}"
    )]
    UnsafeHeadMismatch {
        /// The unsafe head observed from the node.
        observed_hash: B256,
        /// The unsafe head requested by the caller.
        requested_hash: B256,
    },
    /// The unsafe head input was empty after trimming whitespace.
    #[error("unsafe head hash cannot be empty")]
    EmptyUnsafeHead,
    /// The unsafe head input could not be parsed as a 32-byte hash.
    #[error("parsing unsafe head hash `{raw}`: {message}")]
    InvalidUnsafeHead {
        /// The original unsafe head supplied by the caller.
        raw: String,
        /// The parser error returned by the underlying hash parser.
        message: String,
    },
    /// The unsafe head input was the zero hash.
    #[error("unsafe head hash must not be zero")]
    ZeroUnsafeHead {
        /// The parsed zero hash requested by the caller.
        requested_hash: B256,
    },
    /// EL readiness status required before sequencing could not be observed.
    #[error("execution-layer readiness for {node} is unavailable: missing {field}")]
    ExecutionLayerStatusUnavailable {
        /// The target node name.
        node: String,
        /// The missing status field.
        field: &'static str,
    },
    /// The target EL is still syncing.
    #[error(
        "execution layer for {node} is still syncing at block {el_block:?}; required unsafe L2 block is {required_l2_block}"
    )]
    ExecutionLayerSyncing {
        /// The target node name.
        node: String,
        /// The latest observed EL block, if available.
        el_block: Option<u64>,
        /// The unsafe L2 block that the EL must contain before sequencing.
        required_l2_block: u64,
    },
    /// The target EL has stopped syncing but has not reached the required unsafe head.
    #[error(
        "execution layer for {node} is at block {el_block}, behind required unsafe L2 block {required_l2_block}"
    )]
    ExecutionLayerBehind {
        /// The target node name.
        node: String,
        /// The latest observed EL block.
        el_block: u64,
        /// The unsafe L2 block that the EL must contain before sequencing.
        required_l2_block: u64,
    },
    /// The sequencer active state did not converge after the command RPC succeeded.
    #[error("{0}")]
    StateConvergenceTimeout(#[source] Box<StateConvergenceTimeoutError>),
}

/// Error returned when the sequencer active state does not converge after a command RPC succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "{action} RPC succeeded on {node} ({cl_rpc}), but `sequencer_active={expected_active}` was not observed within {timeout:?}; unsafe_head={unsafe_head:?}; last_observed={last_observed:?}; last_error={last_error:?}"
)]
pub struct StateConvergenceTimeoutError {
    /// The sequencer action being observed.
    pub action: &'static str,
    /// The target node name.
    pub node: String,
    /// The target node consensus-layer RPC URL.
    pub cl_rpc: String,
    /// The unsafe head returned or requested for the command, if known.
    pub unsafe_head: Option<B256>,
    /// The expected `sequencer_active` state.
    pub expected_active: bool,
    /// The observation timeout used for state convergence.
    pub timeout: Duration,
    /// The last observed `sequencer_active` state, if any poll succeeded.
    pub last_observed: Option<bool>,
    /// The last polling error, if any poll failed.
    pub last_error: Option<String>,
}

/// Error returned by doctor argument validation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DoctorArgsError {
    /// The head-lag warning threshold is greater than or equal to the failure threshold.
    #[error("`--head-lag-warn-blocks` must be less than `--head-lag-fail-blocks`")]
    HeadLagWarnMustBeLessThanFail {
        /// The configured warning threshold.
        warn_blocks: u64,
        /// The configured failure threshold.
        fail_blocks: u64,
    },
    /// The safe-head recency warning threshold is greater than or equal to the failure threshold.
    #[error("`--safe-recency-warn-blocks` must be less than `--safe-recency-fail-blocks`")]
    SafeRecencyWarnMustBeLessThanFail {
        /// The configured warning threshold.
        warn_blocks: u64,
        /// The configured failure threshold.
        fail_blocks: u64,
    },
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn rpc_error_variants_preserve_source_chain() {
        let proofs = ProofsCommandError::Rpc {
            endpoint: "http://prover:8080/".to_string(),
            method: "prover_getProof",
            source: ProverServiceClientError::ProofFailure {
                message: "witness generation failed".to_string(),
            },
        };
        assert_eq!(
            proofs.to_string(),
            "prover-service request `prover_getProof` failed against http://prover:8080/"
        );
        assert_eq!(
            proofs.source().expect("chained source").to_string(),
            "proof failed: witness generation failed"
        );

        let txpool = TxpoolCommandError::AdminRpc {
            rpc: "http://el:8545/".to_string(),
            method: "admin_dropTransaction",
            source: JsonRpcClientError::Custom("connection refused".to_string()),
        };
        assert_eq!(
            txpool.to_string(),
            "admin RPC method `admin_dropTransaction` failed on http://el:8545/"
        );
        assert!(
            txpool.source().expect("chained source").to_string().contains("connection refused")
        );
    }

    #[test]
    fn node_lookup_errors_name_the_failing_command_group() {
        let missing_source = NodeLookupError::MissingSource { config_name: "devnet".to_string() };
        let missing_node = NodeLookupError::MissingNode {
            requested_node: "op-conductor-1".to_string(),
            available_nodes: vec!["op-conductor-0".to_string()],
        };

        assert!(
            ConductorCommandError::from(missing_source.clone())
                .to_string()
                .starts_with("conductor commands need conductor config")
        );
        assert!(
            SequencerCommandError::from(missing_source)
                .to_string()
                .starts_with("sequencer commands need conductor config")
        );
        assert_eq!(
            ConductorCommandError::from(missing_node.clone()).to_string(),
            "conductor node op-conductor-1 not found. Available nodes: op-conductor-0"
        );
        assert_eq!(
            SequencerCommandError::from(missing_node).to_string(),
            "sequencer node op-conductor-1 not found. Available nodes: op-conductor-0"
        );
    }
}
