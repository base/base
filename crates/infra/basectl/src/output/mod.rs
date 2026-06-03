//! Output formatting and rendering utilities for basectl.

mod format;
pub use format::{
    COLOR_ACTIVE_BORDER, COLOR_BASE_BLUE, COLOR_BURN, COLOR_GAS_FILL, COLOR_GROWTH,
    COLOR_ROW_HIGHLIGHTED, COLOR_ROW_SELECTED, COLOR_TARGET, backlog_size_color, block_color,
    block_color_bright, format_bytes, format_duration, format_gas, format_gwei, format_rate,
    target_usage_color, time_diff_color, truncate_block_number,
};

mod render;
pub use render::{
    L1BlocksTableParams, build_gas_bar, render_da_backlog_bar, render_gas_usage_bar,
    render_l1_blocks_table,
};
