//! CLI parsing and command argument types for basectl.

mod block;
pub use block::{BlockCommand, BlockSummaryJson};

mod cli;
pub use cli::{
    Cli, Commands, ConductorClusterActionArgs, ConductorCommands, ConductorLeaderArgs,
    ConductorNodeActionArgs, ConductorStatusArgs, MonitorCommands, SequencerCommands,
    SequencerNodeActionArgs, SequencerStartArgs, SequencerStatusArgs,
};

mod doctor;
pub use doctor::DoctorCommand;

mod p2p;
pub use p2p::{
    AddTarget, BanAction, DestructiveClBulkArgs, DestructivePeerArgs, P2pArgs, P2pCommand,
    P2pCommands, PeerAction, PeerActionJson, PeerBulkActionResultJson, PeerLayer, PeerTarget,
    PeersJson,
};

mod proofs;
pub use proofs::{
    ProofResultJson, ProofStatusFilter, ProofSummaryJson, ProofsCommand, ProofsCommands,
    ProofsFinalizeArgs, ProofsFinalizeJson, ProofsListArgs, ProofsListJson, ProofsStatusArgs,
    ProofsStatusJson,
};

mod sync_status;
pub use sync_status::{
    ElSyncInfoJson, HeadJson, SyncStatusCommand, SyncStatusJson, TipReferenceJson, TipStatus,
};

mod txpool;
pub use txpool::{
    TxpoolClearArgs, TxpoolClearJson, TxpoolCommand, TxpoolCommands, TxpoolReadArgs, TxpoolReadJson,
};
