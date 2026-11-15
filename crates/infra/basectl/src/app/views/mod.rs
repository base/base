//! TUI view components for basectl panels and dashboards.

mod command_center;
pub use command_center::CommandCenterView;

mod conductor;
pub use conductor::{
    ActionMenuItem, ConductorView, ConfirmButton, MenuAvailability, MenuMode, Overlay,
    PendingAction,
};

mod conductor_gate;
pub use conductor_gate::{GateEvent, GateOverlay, GateProposals};

#[cfg(test)]
mod conductor_test_utils;

mod config;
pub use config::ConfigView;

mod da_monitor;
pub use da_monitor::DaMonitorView;

mod factory;
pub use factory::create_view;

mod home;
pub use home::HomeView;

mod pods;
pub use pods::PodsView;

mod proofs;
pub use proofs::ProofsView;

mod transaction_pane;
pub use transaction_pane::TransactionPane;

mod upgrades;
pub use upgrades::UpgradesView;
