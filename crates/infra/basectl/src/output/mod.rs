//! Output formatting and rendering utilities for basectl.

mod format;
pub use format::{
    COLOR_ACTIVE_BORDER, COLOR_BASE_BLUE, COLOR_BURN, COLOR_GAS_FILL, COLOR_GROWTH,
    COLOR_ROW_HIGHLIGHTED, COLOR_ROW_SELECTED, COLOR_TARGET, backlog_size_color, block_color,
    block_color_bright, format_bytes, format_duration, format_gas, format_gwei, format_rate,
    format_unix_timestamp, target_usage_color, time_diff_color, truncate_block_number,
};

mod json;
pub use json::{JsonOutput, TimestampJson};

mod p2p;
pub use p2p::{P2pInfoJson, P2pInfoTable, P2pLayerInfoJson};

mod render;
pub use render::{
    L1BlocksTableParams, build_gas_bar, render_da_backlog_bar, render_gas_usage_bar,
    render_l1_blocks_table,
};

mod table;
pub use table::KeyValueTable;
