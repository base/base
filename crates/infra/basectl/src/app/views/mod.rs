//! TUI view components for basectl panels and dashboards.

mod command_center;
pub use command_center::CommandCenterView;

mod conductor;
pub use conductor::{ActionMenuItem, ConductorView, ConfirmButton, Overlay, PendingAction};

mod config;
pub use config::ConfigView;

mod da_monitor;
pub use da_monitor::DaMonitorView;

mod factory;
pub use factory::create_view;

mod flashblocks;
pub use flashblocks::FlashblocksView;

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
