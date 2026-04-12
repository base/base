//! Core application logic, actions, resources, and routing for basectl.

mod action;
pub use action::Action;

mod core;
pub use core::App;

mod resources;
pub use resources::{
    ConductorState, DaState, FlashState, LoadTestTask, ProofsState, Resources, ValidatorState,
};

mod router;
pub use router::{Router, ViewId};

mod runner;
pub use runner::{run_app, run_flashblocks_json, start_background_services};

mod view;
pub use view::View;

/// TUI view implementations.
mod views;
pub use views::{
    CommandCenterView, ConductorView, ConfigView, DaMonitorView, FlashblocksView, HomeView,
    LoadTestView, ProofsView, TransactionPane, UpgradesView, create_view,
};
