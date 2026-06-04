//! Core application logic, actions, resources, and routing for basectl.

mod action;
pub use action::Action;

mod core;
pub use core::App;

mod resources;
pub use resources::{
    ConductorState, DaState, FlashState, ProofsState, Resources, SourceLabel, ValidatorState,
};

mod router;
pub use router::{Router, ViewId};

mod runner;
pub use runner::{run_app, run_flashblocks_json, start_background_services};

mod state;
pub use state::{
    BLOB_SIZE, BlockContribution, DaTracker, EVENT_POLL_TIMEOUT, FlashblockEntry, L1_BLOCK_WINDOW,
    L1Block, L1BlockFilter, LoadingState, MAX_HISTORY, RATE_WINDOW_2M, RATE_WINDOW_5M,
    RATE_WINDOW_30S, RateTracker,
};

mod view;
pub use view::View;

/// TUI view implementations.
mod views;
pub use views::{
    ActionMenuItem, CommandCenterView, ConductorView, ConfigView, ConfirmButton, DaMonitorView,
    FlashblocksView, HomeView, Overlay, PendingAction, ProofsView, TransactionPane, UpgradesView,
    create_view,
};
