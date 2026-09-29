#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod feed;
pub use feed::{
    ChainlinkFeed, FeedAnswer, FeedReadError, FeedResolveError, IChainlinkAggregator,
    IChainlinkProxy, InvalidAnswer,
};

mod layout;
pub use layout::ChainlinkLayout;

mod path;
pub use path::{PriceError, PriceLeg, PricePath, PriceQuote};

mod rate;
pub use rate::TokenRate;
