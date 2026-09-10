//! Core application logic, actions, resources, and routing for basectl.

mod action;
pub use action::Action;

mod core;
pub use core::App;

mod resources;
pub use resources::{
    ConductorState, DaState, PodsState, ProofsState, Resources, SourceLabel, ValidatorState,
};

mod router;
pub use router::{Router, ViewId};

mod runner;
pub use runner::{run_app, start_background_services};

mod state;
pub use state::{
    BLOB_SIZE, BlockContribution, DaTracker, EVENT_POLL_TIMEOUT, L1_BLOCK_WINDOW, L1Block,
    L1BlockFilter, LoadingState, MAX_HISTORY, RATE_WINDOW_2M, RATE_WINDOW_5M, RATE_WINDOW_30S,
    RateTracker,
};

mod view;
pub use view::View;

/// TUI view implementations.
mod views;
pub use views::{
    ActionMenuItem, ConductorView, ConfigView, ConfirmButton, DaMonitorView, HomeView, Overlay,
    PendingAction, PodsView, ProofsView, TransactionPane, UpgradesView, create_view,
};
