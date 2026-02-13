mod action;
pub(crate) use action::Action;

mod core;
pub(crate) use core::App;

mod resources;
pub(crate) use resources::Resources;

mod router;
pub(crate) use router::Router;
pub use router::ViewId;

mod runner;
pub use runner::{run_app, run_app_with_view};

mod view;
pub(crate) use view::View;

/// TUI view implementations.
mod views;
