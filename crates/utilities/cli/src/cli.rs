//! CLI parsing utilities.

/// Initializes common process-level setup: backtraces and signal handlers.
#[macro_export]
macro_rules! init_common {
    () => {
        $crate::Backtracing::enable();
        $crate::SigsegvHandler::install();
    };
}

/// Parses CLI arguments with package version and description from the calling crate.
#[macro_export]
macro_rules! parse_cli {
    ($cli_type:ty) => {{ $crate::parse_cli!($cli_type, |cmd| cmd) }};
    ($cli_type:ty, $customize:expr) => {{
        use clap::{CommandFactory, FromArgMatches};

        let pkg_name = env!("CARGO_PKG_NAME");
        let version = env!("CARGO_PKG_VERSION");
        let description = env!("CARGO_PKG_DESCRIPTION");

        let cmd = <$cli_type>::command()
            .version(version)
            .long_version(version)
            .about(description)
            .name(pkg_name);
        let cmd = ($customize)(cmd);
        let matches = cmd.get_matches();
        <$cli_type>::from_arg_matches(&matches).expect("Parsing args")
    }};
}
