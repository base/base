use super::{
    ConductorView, ConfigView, DaMonitorView, HomeView, PodsView, ProofsView, UpgradesView,
};
use crate::app::{View, ViewId};

/// Creates a boxed view instance for the given view identifier.
pub fn create_view(view_id: ViewId) -> Box<dyn View> {
    match view_id {
        ViewId::Home => Box::new(HomeView::new()),
        ViewId::Conductor => Box::new(ConductorView::new()),
        ViewId::DaMonitor => Box::new(DaMonitorView::new()),
        ViewId::Config => Box::new(ConfigView::new()),
        ViewId::Proofs => Box::new(ProofsView::new()),
        ViewId::Pods => Box::new(PodsView::new()),
        ViewId::Upgrades => Box::new(UpgradesView::new()),
    }
}
