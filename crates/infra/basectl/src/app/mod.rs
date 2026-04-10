//! Core application logic, actions, resources, and routing for basectl.

mod action;
pub use action::Action;

mod core;
pub use core::App;

mod resources;
pub use resources::{LoadTestTask, Resources};

mod router;
pub use router::Router;
pub use router::ViewId;

mod runner;
pub use runner::{run_app, run_flashblocks_json};

mod view;
pub use view::View;

/// TUI view implementations.
mod views;
