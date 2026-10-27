//! Curated flag surface for the integrated `base` node commands.

use std::ffi::OsString;

use clap::{Arg, ArgAction, ArgMatches, Command, error::ErrorKind, parser::ValueSource};
use tracing::warn;

use crate::config::ChainArg;

/// Base-owned visibility, naming, and rejection rules layered over the derived `base` CLI.
///
/// `base rpc` and `base sequencer` flatten the full execution, consensus, and builder argument
/// sets. Only the curated and advanced flags below are supported: `-h`/`--help` lists the curated
/// ones, `--help-all` adds the advanced ones, and every other flag is hidden and ignored with a
/// startup warning, whether it comes from the command line or its environment variable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CliSurface;

impl CliSurface {
    /// Node subcommands that take `--chain` and accept the renamed flags.
    pub(crate) const NODE_COMMANDS: [&str; 7] =
        ["rpc", "sequencer", "follow", "bootnode", "dev", "execution", "consensus"];

    /// Node subcommands that accept only curated and advanced flags.
    pub(crate) const CURATED_COMMANDS: [&str; 2] = ["rpc", "sequencer"];

    /// Flags renamed to Base conventions. The old spelling stays accepted as a hidden alias.
    pub(crate) const RENAMES: &[(&str, &str)] = &[
        ("l1-eth-rpc", "l1.rpc"),
        ("l1-beacon", "l1.beacon"),
        ("l1-trust-rpc", "l1.trust-rpc"),
        ("l1-slot-duration-override", "l1.slot-duration-override"),
        ("l1-config-file", "l1.config-file"),
        ("l2-config-file", "l2.config-file"),
        ("builder.max_gas_per_txn", "builder.max-gas-per-txn"),
        ("rpc.disabled", "consensus.rpc.disabled"),
        ("rpc.no-restart", "consensus.rpc.no-restart"),
        ("rpc.addr", "consensus.rpc.addr"),
        ("rpc.port", "consensus.rpc.port"),
        ("rpc.enable-admin", "consensus.rpc.enable-admin"),
        ("rpc.admin-state", "consensus.rpc.admin-state"),
        ("rpc.ws-enabled", "consensus.rpc.ws-enabled"),
        ("rpc.dev-enabled", "consensus.rpc.dev-enabled"),
        ("rpc.timeout", "consensus.rpc.timeout"),
        ("rpc.max-concurrent", "consensus.rpc.max-concurrent"),
    ];

    /// Flags that cannot apply when consensus calls the embedded execution node's Engine API in
    /// memory: the auth port and address are overwritten, no socket or JWT is used between the
    /// two layers, and disabling the auth server would remove the in-memory auth module.
    ///
    /// These are ignored like any other unsupported flag, with an Engine API specific warning.
    pub(crate) const UNSUPPORTED: &[&str] = &[
        "authrpc.addr",
        "authrpc.port",
        "authrpc.jwtsecret",
        "auth-ipc",
        "disable-auth-server",
        "l2-engine-jwt-secret",
        "l2-engine-jwt-encoded",
        "l2-engine-timeout",
    ];

    /// Help heading for flags listed in the default top section.
    pub(crate) const DEFAULT_HEADING: &str = "Options";

    /// Curated flags shared by every node command, with their help heading.
    pub(crate) const NODE_FLAGS: &[(&str, &str)] = &[
        ("chain", Self::DEFAULT_HEADING),
        ("l1.rpc", Self::DEFAULT_HEADING),
        ("l1.beacon", Self::DEFAULT_HEADING),
        ("datadir", "Storage"),
        ("full", "Storage"),
        ("http", "JSON-RPC"),
        ("http.addr", "JSON-RPC"),
        ("http.port", "JSON-RPC"),
        ("http.api", "JSON-RPC"),
        ("http.corsdomain", "JSON-RPC"),
        ("ws", "JSON-RPC"),
        ("ws.addr", "JSON-RPC"),
        ("ws.port", "JSON-RPC"),
        ("ws.api", "JSON-RPC"),
        ("ws.origins", "JSON-RPC"),
        ("consensus.rpc.addr", "Consensus RPC"),
        ("consensus.rpc.port", "Consensus RPC"),
        ("consensus.rpc.enable-admin", "Consensus RPC"),
        ("port", "Execution P2P"),
        ("discovery.port", "Execution P2P"),
        ("addr", "Execution P2P"),
        ("nat", "Execution P2P"),
        ("bootnodes", "Execution P2P"),
        ("trusted-peers", "Execution P2P"),
        ("max-peers", "Execution P2P"),
        ("p2p-secret-key", "Execution P2P"),
        ("p2p.listen.ip", "Consensus P2P"),
        ("p2p.listen.tcp", "Consensus P2P"),
        ("p2p.listen.udp", "Consensus P2P"),
        ("p2p.advertise.ip", "Consensus P2P"),
        ("p2p.bootnodes", "Consensus P2P"),
        ("p2p.priv.path", "Consensus P2P"),
        ("p2p.peers.hi", "Consensus P2P"),
        ("metrics", "Metrics"),
    ];

    /// Curated flags only on `base sequencer`.
    pub(crate) const SEQUENCER_FLAGS: &[(&str, &str)] = &[
        ("sequencer.stopped", "Sequencer"),
        ("sequencer.recover", "Sequencer"),
        ("p2p.sequencer.key.path", "Sequencer"),
        ("p2p.signer.endpoint", "Sequencer"),
        ("p2p.signer.address", "Sequencer"),
        ("conductor.rpc", "Sequencer"),
    ];

    /// Advanced flags supported on both curated commands, listed only by `--help-all`.
    pub(crate) const ADVANCED_FLAGS: &[&str] = &[
        "execution-chain",
        "l1.trust-rpc",
        "l1.config-file",
        "l2.config-file",
        "l1.slot-duration-override",
        "l1.verifier-confs",
        "l1.dangerously-override-da-batch-inbox",
        "l1.dangerously-override-da-batcher-sender",
        "txpool.nolocals",
        "txpool.max-account-slots",
        "txpool.max-batch-size",
        "txpool.pending-max-count",
        "txpool.pending-max-size",
        "txpool.basefee-max-count",
        "txpool.basefee-max-size",
        "txpool.queued-max-count",
        "txpool.queued-max-size",
        "rollup.txpool-max-inflight-delegated-slots",
        "rpc.gascap",
        "rpc.txfeecap",
        "rpc.eth-proof-window",
        "rpc.execution-forwarding-endpoint",
        "ipcpath",
        "p2p-secret-key-hex",
        "disable-discovery",
        "rollup.discovery.v4",
        "p2p.bootnodes-file",
        "p2p.scoring",
        "upgrade-signal.contract",
        "upgrade-signal.mode",
        "upgrade-signal.l1-block-tag",
        "upgrade-signal.l1-rpc",
        "payload.resource-metering-schedule",
        "validity-signature-mode",
        "enable-shadow-indexer",
        "shadow-indexer.db-host",
        "shadow-indexer.db-port",
        "shadow-indexer.db-name",
        "shadow-indexer.db-user",
        "shadow-indexer.db-password",
        "shadow-indexer.max-connections",
        "shadow-indexer.connection-timeout",
        "shadow-indexer.retention",
        "shadow-indexer.retention-interval",
    ];

    /// Advanced flags supported only on `base rpc`.
    pub(crate) const RPC_ADVANCED_FLAGS: &[&str] = &[
        "rpc.forwarding-endpoint",
        "enable-tx-forwarding",
        "builder-rpc-urls",
        "rollup.payer-enabled",
        "rollup.payer",
        "enable-metering",
        "metering.metered-opcodes",
        "validity-max-predicates",
        "validity-max-expiry-secs",
        "enable-transaction-event-journal",
        "flashblocks-url",
    ];

    /// Advanced flags supported only on `base sequencer`.
    pub(crate) const SEQUENCER_ADVANCED_FLAGS: &[&str] = &[
        "p2p.sequencer.key",
        "p2p.signer.header",
        "p2p.signer.tls.ca",
        "p2p.signer.tls.cert",
        "p2p.signer.tls.key",
        "conductor.rpc.timeout",
        "conductor.binary-commit",
        "sequencer.l1-confs",
        "sequencer.l1-rpc-timeout-ms",
        "rollup.sequencer",
        "rollup.sequencer-headers",
        "rollup.chain-block-time",
        "builder.max-gas-per-txn",
        "builder.enable-resource-metering",
        "builder.payload-builder-cutover",
        "builder.transaction-events.enabled",
        "builder.transaction-events.network",
        "builder.transaction-events.file-path",
        "telemetry.sampling-ratio",
        "flashblocks.addr",
        "flashblocks.port",
        "flashblocks.block-time",
        "flashblocks.leeway-time",
    ];

    /// Help text replacing upstream text that is ambiguous in a combined node or leaks rustdoc.
    pub(crate) const HELP: &[(&str, &str)] = &[
        ("datadir", "Directory for all node data"),
        ("full", "Run a pruned full node instead of an archive node"),
        ("metrics", "Serve execution Prometheus metrics on this address"),
        ("port", "Execution P2P (devp2p) TCP listen port"),
        ("addr", "Execution P2P listen address"),
        ("discovery.port", "Execution P2P discovery (discv4) UDP port"),
        ("p2p-secret-key", "File holding the execution P2P secret key"),
        ("consensus.rpc.addr", "Consensus RPC (`optimism_*`, `admin_*`) listen address"),
        ("consensus.rpc.port", "Consensus RPC (`optimism_*`, `admin_*`) listen port"),
        ("consensus.rpc.enable-admin", "Enable the consensus `admin_*` RPC namespace"),
    ];

    /// Value names replacing upstream field-derived placeholders.
    pub(crate) const VALUE_NAMES: &[(&str, &str)] = &[
        ("l1.rpc", "URL"),
        ("l1.beacon", "URL"),
        ("metrics", "IP:PORT"),
        ("consensus.rpc.addr", "IP"),
        ("consensus.rpc.port", "PORT"),
        ("p2p.bootnodes", "ENRS"),
    ];

    /// Returns the command-specific curated flags for a node command.
    pub(crate) fn command_flags(name: &str) -> &'static [(&'static str, &'static str)] {
        match name {
            "sequencer" => Self::SEQUENCER_FLAGS,
            _ => &[],
        }
    }

    /// Returns the command-specific advanced flags for a node command.
    pub(crate) fn command_advanced_flags(name: &str) -> &'static [&'static str] {
        match name {
            "rpc" => Self::RPC_ADVANCED_FLAGS,
            "sequencer" => Self::SEQUENCER_ADVANCED_FLAGS,
            _ => &[],
        }
    }

    /// Returns whether a curated node command supports a flag, by its long name.
    pub(crate) fn supports(name: &str, long: &str) -> bool {
        Self::NODE_FLAGS.iter().chain(Self::command_flags(name)).any(|(flag, _)| *flag == long)
            || Self::ADVANCED_FLAGS
                .iter()
                .chain(Self::command_advanced_flags(name))
                .any(|flag| *flag == long)
    }

    /// Applies the curated surface to the derived `base` command.
    pub(crate) fn apply(command: Command) -> Command {
        let command = command.mut_arg("base_chain", |arg| arg.hide(true));
        let command = Self::NODE_COMMANDS.into_iter().fold(command, |command, name| {
            command.mut_subcommand(name, |sub| {
                Self::map_args(sub.arg(Self::chain_arg()), Self::rename_arg)
            })
        });
        Self::CURATED_COMMANDS.into_iter().fold(command, |command, name| {
            command.mut_subcommand(name, |sub| Self::curate(sub, name))
        })
    }

    /// Builds the `--chain` argument attached to each node subcommand.
    pub(crate) fn chain_arg() -> Arg {
        Arg::new("chain")
            .long("chain")
            .short('c')
            .value_name("CHAIN")
            .env("BASE_CHAIN")
            .value_parser(clap::value_parser!(ChainArg))
            .display_order(0)
            .help(
                "Chain to run: `mainnet`, `sepolia`, `zeronet`, `dev`, a canonical name such as \
                 `base-sepolia`, an L2 chain ID, or a chain TOML file",
            )
    }

    /// Returns the subcommand `--chain` when it was supplied on the command line.
    pub(crate) fn subcommand_cli_chain(matches: &ArgMatches) -> Option<ChainArg> {
        let (name, sub_matches) = matches.subcommand()?;
        (Self::NODE_COMMANDS.contains(&name)
            && sub_matches.value_source("chain") == Some(ValueSource::CommandLine))
        .then(|| sub_matches.get_one::<ChainArg>("chain").cloned())
        .flatten()
    }

    /// Applies `f` to every argument of a command.
    pub(crate) fn map_args(command: Command, f: impl Fn(Arg) -> Arg) -> Command {
        let ids: Vec<_> = command.get_arguments().map(|arg| arg.get_id().clone()).collect();
        ids.into_iter().fold(command, |command, id| command.mut_arg(id, &f))
    }

    /// Hides, annotates, and adds `--help-all` to one curated node command.
    pub(crate) fn curate(command: Command, name: &'static str) -> Command {
        Self::map_args(command, |arg| Self::curate_arg(arg, name))
            .disable_help_flag(true)
            .arg(
                Arg::new("help")
                    .short('h')
                    .long("help")
                    .action(ArgAction::HelpShort)
                    .help("Print help")
                    .help_heading(None),
            )
            .arg(
                Arg::new("help_all")
                    .long("help-all")
                    .action(ArgAction::HelpLong)
                    .help("Print help, including advanced flags")
                    .help_heading(None),
            )
    }

    /// Renames an argument listed in [`Self::RENAMES`].
    pub(crate) fn rename_arg(arg: Arg) -> Arg {
        let renamed =
            arg.get_long().and_then(|long| Self::RENAMES.iter().find(|(old, _)| *old == long));
        match renamed {
            Some((old, new)) => Self::rename(arg, old, new),
            None => arg,
        }
    }

    /// Applies visibility, heading, and help rules to a single argument.
    pub(crate) fn curate_arg(arg: Arg, name: &str) -> Arg {
        let Some(long) = arg.get_long().map(str::to_owned) else {
            return arg.hide_short_help(true);
        };
        if !Self::supports(name, &long) {
            return arg.hide(true).env(None::<&str>);
        }
        let curated = Self::NODE_FLAGS
            .iter()
            .chain(Self::command_flags(name))
            .find(|(flag, _)| *flag == long);
        let Some((_, heading)) = curated else {
            return arg.hide_short_help(true);
        };
        let mut arg = arg.help_heading((*heading != Self::DEFAULT_HEADING).then_some(*heading));
        if let Some((_, help)) = Self::HELP.iter().find(|(flag, _)| *flag == long) {
            arg = arg.help(*help).long_help(None);
        }
        if let Some((_, value_name)) = Self::VALUE_NAMES.iter().find(|(flag, _)| *flag == long) {
            arg = arg.value_name(*value_name);
        }
        arg
    }

    /// Renames a flag, demoting the old spelling and any upstream aliases to hidden aliases.
    pub(crate) fn rename(arg: Arg, old: &'static str, new: &'static str) -> Arg {
        let aliases: Vec<String> = arg
            .get_all_aliases()
            .unwrap_or_default()
            .into_iter()
            .filter(|alias| *alias != new)
            .map(str::to_owned)
            .collect();
        aliases
            .into_iter()
            .fold(arg.alias(None).long(new).alias(old), |arg, alias| arg.alias(alias))
    }

    /// Returns the long name a flag is known by after [`Self::RENAMES`].
    pub(crate) fn canonical_long(arg: &Arg) -> Option<&str> {
        let long = arg.get_long()?;
        Some(Self::RENAMES.iter().find(|(old, _)| *old == long).map_or(long, |(_, new)| *new))
    }

    /// Returns whether a command-line token such as `--flag` or `-f` names `arg`.
    pub(crate) fn names(arg: &Arg, flag: &str) -> bool {
        let is_long = |name: &str| flag.strip_prefix("--") == Some(name);
        let mut short = flag.chars();
        arg.get_long().is_some_and(is_long)
            || Self::canonical_long(arg).is_some_and(is_long)
            || arg.get_all_aliases().unwrap_or_default().into_iter().any(is_long)
            || arg.get_short().is_some_and(|s| {
                short.next() == Some('-') && short.next() == Some(s) && short.next().is_none()
            })
    }

    /// Removes unsupported flags of a curated node command from `args`, returning the remaining
    /// arguments and every ignored flag, including unsupported flags whose environment variable is
    /// set. `command` is the derived command before [`Self::apply`].
    pub(crate) fn strip_unsupported(
        command: &Command,
        args: Vec<OsString>,
    ) -> (Vec<OsString>, Vec<IgnoredFlag>) {
        // ponytail: the subcommand is the first token naming a curated command, so a root flag
        // value spelled `rpc` or `sequencer` would be misread; clustered or attached short flags
        // (`-ab`, `-a1`) reach clap unchanged. Pre-parse with clap if either ever matters.
        let position = args
            .iter()
            .skip(1)
            .position(|arg| Self::CURATED_COMMANDS.iter().any(|name| arg == name))
            .map(|index| index + 1);
        let Some((position, name)) =
            position.and_then(|position| Some((position, args[position].to_str()?)))
        else {
            return (args, Vec::new());
        };
        let Some(sub) = command.find_subcommand(name) else {
            return (args, Vec::new());
        };
        let unsupported: Vec<(&Arg, &str)> = sub
            .get_arguments()
            .filter_map(|arg| Some((arg, Self::canonical_long(arg)?)))
            .filter(|(_, long)| !Self::supports(name, long))
            .collect();

        let mut ignored: Vec<IgnoredFlag> = unsupported
            .iter()
            .filter_map(|(arg, long)| {
                let env = arg.get_env()?;
                std::env::var_os(env)?;
                Some(IgnoredFlag {
                    long: (*long).to_owned(),
                    env: Some(env.to_string_lossy().into_owned()),
                })
            })
            .collect();
        let mut kept = args[..=position].to_vec();
        let mut rest = args[position + 1..].iter().peekable();
        while let Some(token) = rest.next() {
            if token == "--" {
                kept.push(token.clone());
                kept.extend(rest.cloned());
                break;
            }
            let text = token.to_string_lossy();
            let (flag, inline_value) =
                text.split_once('=').map_or((&*text, false), |(f, _)| (f, true));
            let Some((arg, long)) = unsupported.iter().find(|(arg, _)| Self::names(arg, flag))
            else {
                kept.push(token.clone());
                continue;
            };
            ignored.push(IgnoredFlag { long: (*long).to_owned(), env: None });
            if inline_value || !arg.get_action().takes_values() {
                continue;
            }
            let range = arg.get_num_args().unwrap_or_else(|| 1.into());
            let mut taken = 0;
            while taken < range.max_values()
                && rest.peek().is_some_and(|next| {
                    taken < range.min_values() || !next.to_string_lossy().starts_with('-')
                })
            {
                rest.next();
                taken += 1;
            }
        }
        (kept, ignored)
    }

    /// Rejects node command lines that pass `--chain` both before and after the subcommand.
    pub(crate) fn validate(command: &mut Command, matches: &ArgMatches) -> Result<(), clap::Error> {
        let Some((name, sub_matches)) = matches.subcommand() else {
            return Ok(());
        };
        if !Self::NODE_COMMANDS.contains(&name) {
            return Ok(());
        }
        let Some(sub) = command.find_subcommand_mut(name) else {
            return Ok(());
        };
        let from_cli = |matches: &ArgMatches, id: &str| {
            matches.value_source(id) == Some(ValueSource::CommandLine)
        };
        if from_cli(matches, "base_chain") && from_cli(sub_matches, "chain") {
            return Err(sub.error(
                ErrorKind::ArgumentConflict,
                format!("`--chain` was given both before and after `{name}`; pass it once"),
            ));
        }
        Ok(())
    }
}

/// A flag a curated node command ignored because it is outside the supported surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IgnoredFlag {
    /// Long name of the flag.
    pub(crate) long: String,
    /// Environment variable that set the flag, or `None` when it came from the command line.
    pub(crate) env: Option<String>,
}

impl IgnoredFlag {
    /// Logs that the flag was ignored and why.
    pub(crate) fn warn(&self) {
        if CliSurface::UNSUPPORTED.contains(&self.long.as_str()) {
            warn!(
                flag = %self.long,
                env = ?self.env,
                "ignoring Engine API flag: consensus calls the embedded execution node in memory"
            );
        } else {
            warn!(
                flag = %self.long,
                env = ?self.env,
                "ignoring unsupported flag; run with --help-all to list supported flags"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::PathBuf, process::Command as Process};

    use clap::CommandFactory;

    use super::*;
    use crate::{cli::BaseCli, commands::BaseCommand};

    const L1_ARGS: [&str; 4] =
        ["--l1.rpc", "http://localhost:8545", "--l1.beacon", "http://localhost:5052"];
    const ENV_CHILD_TEST: &str = "surface::tests::env_values_never_fail_or_override_cli_child";

    fn curated() -> Command {
        let mut command = CliSurface::apply(BaseCli::command());
        command.build();
        command
    }

    fn longs(command: &Command, name: &str) -> HashSet<String> {
        command
            .find_subcommand(name)
            .unwrap()
            .get_arguments()
            .filter_map(Arg::get_long)
            .map(str::to_owned)
            .collect()
    }

    fn parse(args: &[&str]) -> Result<BaseCli, clap::Error> {
        BaseCli::try_parse_curated(args.iter().copied().chain(L1_ARGS))
    }

    #[test]
    fn surface_tables_name_existing_flags() {
        let command = curated();
        let rpc = longs(&command, "rpc");
        let sequencer = longs(&command, "sequencer");

        for (flag, _) in CliSurface::NODE_FLAGS {
            assert!(rpc.contains(*flag) && sequencer.contains(*flag), "missing --{flag}");
        }
        for (flag, _) in CliSurface::SEQUENCER_FLAGS {
            assert!(sequencer.contains(*flag), "missing sequencer --{flag}");
        }
        for flag in CliSurface::UNSUPPORTED.iter().chain(CliSurface::ADVANCED_FLAGS) {
            assert!(rpc.contains(*flag) && sequencer.contains(*flag), "missing --{flag}");
        }
        for flag in CliSurface::RPC_ADVANCED_FLAGS {
            assert!(rpc.contains(*flag), "missing rpc --{flag}");
        }
        for flag in CliSurface::SEQUENCER_ADVANCED_FLAGS {
            assert!(sequencer.contains(*flag), "missing sequencer --{flag}");
        }
        let bootnode = longs(&command, "bootnode");
        for (_, new) in CliSurface::RENAMES {
            assert!(
                rpc.contains(*new) || sequencer.contains(*new) || bootnode.contains(*new),
                "missing --{new}"
            );
        }

        let curated: HashSet<_> = CliSurface::NODE_FLAGS
            .iter()
            .chain(CliSurface::SEQUENCER_FLAGS)
            .map(|(flag, _)| *flag)
            .collect();
        for (flag, _) in CliSurface::HELP.iter().chain(CliSurface::VALUE_NAMES) {
            assert!(curated.contains(flag), "override for uncurated --{flag}");
        }
    }

    #[test]
    fn env_vars_are_unique_per_command() {
        let command = curated();
        for name in CliSurface::NODE_COMMANDS {
            let mut seen = HashSet::new();
            for arg in command.find_subcommand(name).unwrap().get_arguments() {
                if let Some(env) = arg.get_env() {
                    assert!(seen.insert(env.to_owned()), "duplicate env {env:?} on `base {name}`");
                }
            }
        }
    }

    #[test]
    fn short_help_lists_only_curated_flags() {
        let mut command = curated();
        let rpc = command.find_subcommand_mut("rpc").unwrap();

        let help = rpc.render_help().to_string();
        assert!(help.contains("--l1.rpc"));
        assert!(help.contains("--consensus.rpc.port"));
        assert!(!help.contains("--engine.persistence-threshold"));
        assert!(!help.contains("--shadow-indexer.db-password"));
        assert!(!help.contains("--authrpc.port"));
        assert!(!help.contains("flashblocks"));

        let help_all = rpc.render_long_help().to_string();
        assert!(help_all.contains("--txpool.max-account-slots"));
        assert!(!help_all.contains("--engine.persistence-threshold"));
        assert!(!help_all.contains("--authrpc.port"));
    }

    #[test]
    fn accepts_advanced_flags() {
        assert!(parse(&["base", "rpc", "--txpool.max-account-slots", "32"]).is_ok());
        assert!(parse(&["base", "rpc", "--enable-metering"]).is_ok());
        assert!(parse(&["base", "sequencer", "--sequencer.l1-confs", "4"]).is_ok());
    }

    fn ignored(cli: &BaseCli) -> Vec<&str> {
        cli.ignored.iter().map(|flag| flag.long.as_str()).collect()
    }

    #[test]
    fn ignores_flags_outside_the_supported_surface() {
        let cli = parse(&[
            "base",
            "rpc",
            "--engine.persistence-threshold",
            "4",
            "--auth-ipc.path=/data/engine.ipc",
            "--disable-dns-discovery",
            "--consensus.rpc.port",
            "9546",
        ])
        .unwrap();

        assert_eq!(
            ignored(&cli),
            ["engine.persistence-threshold", "auth-ipc.path", "disable-dns-discovery"]
        );
        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };
        assert_eq!(rpc.consensus.rpc_flags.listen_port, 9546);
    }

    #[test]
    fn accepts_flags_operators_set_through_env() {
        assert!(parse(&["base", "rpc", "--l1.trust-rpc"]).unwrap().ignored.is_empty());
        let cli = parse(&[
            "base",
            "sequencer",
            "--p2p.signer.endpoint",
            "http://signer:8080",
            "--p2p.signer.address",
            "0x4242424242424242424242424242424242424242",
            "--p2p.signer.tls.ca",
            "/tls/ca.pem",
            "--conductor.rpc.timeout",
            "5",
            "--sequencer.l1-rpc-timeout-ms",
            "500",
            "--flashblocks.leeway-time",
            "50",
        ])
        .unwrap();
        assert!(cli.ignored.is_empty(), "{:?}", cli.ignored);
    }

    #[test]
    fn global_flags_stay_supported() {
        assert!(parse(&["base", "rpc", "-vvv", "--logs.stdout.format", "json"]).is_ok());
    }

    #[test]
    fn renamed_flags_accept_new_and_old_spellings() {
        for args in
            [["base", "rpc", "--consensus.rpc.port", "9546"], ["base", "rpc", "--rpc.port", "9546"]]
        {
            let BaseCommand::Rpc(rpc) = parse(&args).unwrap().command else {
                panic!("expected rpc command");
            };
            assert_eq!(rpc.consensus.rpc_flags.listen_port, 9546);
        }

        let cli = BaseCli::try_parse_curated([
            "base",
            "rpc",
            "--l1-eth-rpc",
            "http://old:8545",
            "--l1-beacon",
            "http://old:5052",
        ])
        .unwrap();
        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };
        assert_eq!(rpc.consensus.l1_rpc_args.l1_eth_rpc.as_str(), "http://old:8545/");
    }

    #[test]
    fn ignores_flags_that_do_not_apply_in_process() {
        for (args, flag) in [
            (&["base", "rpc", "--authrpc.port", "8551"][..], "authrpc.port"),
            (&["base", "rpc", "--l2-engine-jwt-secret", "/jwt.hex"], "l2-engine-jwt-secret"),
            (&["base", "sequencer", "--disable-auth-server"], "disable-auth-server"),
        ] {
            assert_eq!(ignored(&parse(args).unwrap()), [flag]);
        }
    }

    #[test]
    fn bootnode_accepts_renamed_flags() {
        let cli =
            BaseCli::try_parse_curated(["base", "bootnode", "--l2.config-file", "/rollup.json"])
                .unwrap();

        let BaseCommand::Bootnode(bootnode) = cli.command else {
            panic!("expected bootnode command");
        };
        assert_eq!(bootnode.l2_config.l2_config_file, Some(PathBuf::from("/rollup.json")));
    }

    #[test]
    fn rejects_chain_before_and_after_node_command() {
        let err =
            BaseCli::try_parse_curated(["base", "-c", "mainnet", "bootnode", "-c", "sepolia"])
                .unwrap_err();

        assert!(err.to_string().contains("pass it once"), "{err}");
    }

    #[test]
    fn env_values_never_fail_or_override_cli() {
        let output = Process::new(std::env::current_exe().unwrap())
            .args(["--exact", ENV_CHILD_TEST, "--ignored"])
            .env("BASE_CHAIN", "mainnet")
            .env("BASE_NODE_L2_ENGINE_AUTH", "/tmp/jwt.hex")
            .env("BASE_NODE_PORT", "tcp://10.0.0.1:30303")
            .env("BASE_NODE_L1_RPC_TIMEOUT_MS", "not-a-number")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "child env test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "spawned by env_values_never_fail_or_override_cli with isolated process env"]
    fn env_values_never_fail_or_override_cli_child() {
        let cli = parse(&["base", "--chain", "sepolia", "rpc"]).unwrap();
        assert_eq!(cli.chain, Some(ChainArg::BuiltIn("sepolia".to_owned())));

        let cli = parse(&["base", "rpc", "--chain", "sepolia"]).unwrap();
        assert_eq!(cli.chain, Some(ChainArg::BuiltIn("sepolia".to_owned())));

        let cli = parse(&["base", "rpc"]).unwrap();
        assert_eq!(cli.chain, Some(ChainArg::BuiltIn("mainnet".to_owned())));
        assert!(cli.ignored.contains(&IgnoredFlag {
            long: "l1.rpc-timeout-ms".to_owned(),
            env: Some("BASE_NODE_L1_RPC_TIMEOUT_MS".to_owned()),
        }));
    }
}
