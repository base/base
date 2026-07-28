//! CLI parsing and command argument types for basectl.

mod block;
pub use block::{BlockCommand, BlockSummaryJson};

mod cli;
pub use cli::{
    Cli, Commands, ConductorClusterActionArgs, ConductorCommands, ConductorLeaderArgs,
    ConductorNodeActionArgs, ConductorStatusArgs, DestructiveClBulkArgs, DestructivePeerArgs,
    MonitorCommands, P2pArgs, P2pCommands, ProofStatusFilter, ProofsCommands, ProofsFinalizeArgs,
    ProofsListArgs, ProofsStatusArgs, SequencerCommands, SequencerNodeActionArgs,
    SequencerStartArgs, SequencerStatusArgs, TxpoolClearArgs, TxpoolCommands, TxpoolReadArgs,
};

mod doctor;
pub use doctor::DoctorCommand;

mod sync_status;
pub use sync_status::{
    ElSyncInfoJson, HeadJson, SyncStatusCommand, SyncStatusJson, TipReferenceJson, TipStatus,
};
