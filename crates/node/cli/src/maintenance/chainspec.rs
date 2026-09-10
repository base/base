use std::sync::Arc;

use base_common_chain_config::BaseChainSpec;
use clap::builder::TypedValueParser;

#[derive(Debug, Clone)]
/// Clap value parser for the Base chain specification.
pub struct ChainSpecValueParser;

impl TypedValueParser for ChainSpecValueParser {
    type Value = Arc<BaseChainSpec>;

    fn parse_ref(
        &self,
        _cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let val =
            value.to_str().ok_or_else(|| clap::Error::new(clap::error::ErrorKind::InvalidUtf8))?;
        crate::BaseChainSpecParser::parse(val).map_err(|err| {
            let arg = arg.map(|a| a.to_string()).unwrap_or_else(|| "...".to_owned());
            let possible_values = crate::BaseChainSpecParser::SUPPORTED_CHAINS.join(",");
            let msg = format!(
                "Invalid value '{val}' for {arg}: {err}.\n    [possible values: {possible_values}]"
            );
            clap::Error::raw(clap::error::ErrorKind::InvalidValue, msg)
        })
    }
}
