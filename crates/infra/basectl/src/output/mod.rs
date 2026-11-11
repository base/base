//! Output formatting and rendering utilities for basectl.

mod format;
pub use format::{
    COLOR_ACTIVE_BORDER, COLOR_BASE_BLUE, COLOR_BURN, COLOR_GROWTH, COLOR_ROW_HIGHLIGHTED,
    COLOR_ROW_SELECTED, Format,
};

mod json;
pub use json::{JsonOutput, TimestampJson};

mod p2p;
pub use p2p::{P2pClInfoJson, P2pElInfoJson, P2pInfoJson, P2pInfoTable};

mod render;
pub use render::{L1BlocksTableParams, render_da_backlog_bar, render_l1_blocks_table};

mod table;
pub use table::KeyValueTable;
