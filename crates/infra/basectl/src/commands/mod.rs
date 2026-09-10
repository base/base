//! CLI parsing and command argument types for basectl.

mod block;
pub use block::{BlockCommand, BlockSummaryJson};

mod cli;
pub use cli::{Cli, Commands, MonitorCommands};

mod conductor;
pub use conductor::{
    ClusterNodeScope, ConductorAction, ConductorActionJson, ConductorActionName,
    ConductorClusterActionArgs, ConductorCommand, ConductorCommands, ConductorFailureJson,
    ConductorFanoutJson, ConductorLeaderArgs, ConductorNodeActionArgs, ConductorNodeJson,
    ConductorStatusArgs, ConductorStatusJson, PausedSummaryJson,
};

mod doctor;
pub use doctor::DoctorCommand;

mod p2p;
pub use p2p::{
    AddTarget, BanAction, DestructiveClBulkArgs, DestructivePeerArgs, P2pArgs, P2pCommand,
    P2pCommands, PeerAction, PeerActionJson, PeerBulkAction, PeerBulkActionResultJson, PeerLayer,
    PeerTarget, PeersJson,
};

mod node_metrics;
pub use node_metrics::NodeMetricsJson;

mod outcome;
pub use outcome::{CommandOutcome, OptionalValue};

mod proofs;
pub use proofs::{
    ExecutionStatsJson, FinalizeTarget, GameDetailsJson, GameSummaryJson, GamesListJson,
    ProofOutputStatus, ProofResultJson, ProofStatusFilter, ProofSummaryJson, ProofsCommand,
    ProofsCommands, ProofsFinalizeArgs, ProofsGamesArgs, ProofsListArgs, ProofsListJson,
    ProofsProposeArgs, ProofsProposeJson, ProofsStatusArgs, ProofsStatusJson, ProofsSubmitArgs,
    ProofsSubmitJson, ZkBackendOption,
};

mod sequencer;
pub use sequencer::{
    LeadershipStatus, SequencerAction, SequencerActionJson, SequencerCommand, SequencerCommands,
    SequencerNodeActionArgs, SequencerNodeJson, SequencerRole, SequencerStartArgs,
    SequencerStatusArgs, SequencerStatusJson, UnsafeHeadSource,
};

mod sync_status;
pub use sync_status::{
    ElSyncInfoJson, HeadJson, SyncStatusCommand, SyncStatusJson, TipReferenceJson, TipStatus,
};

mod txpool;
pub use txpool::{
    TxpoolClearArgs, TxpoolClearJson, TxpoolCommand, TxpoolCommands, TxpoolReadArgs, TxpoolReadJson,
};

mod upgrade_readiness;
pub use upgrade_readiness::UpgradeReadinessCommand;
