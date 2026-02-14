mod command_center;
pub(crate) use command_center::CommandCenterView;

mod config;
pub(crate) use config::ConfigView;

mod da_monitor;
pub(crate) use da_monitor::DaMonitorView;

mod factory;
pub(crate) use factory::create_view;

mod flashblocks;
pub(crate) use flashblocks::FlashblocksView;

mod home;
pub(crate) use home::HomeView;
