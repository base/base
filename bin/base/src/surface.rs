//! Curated flag surface for the integrated `base` node commands.

use clap::{Arg, ArgAction, ArgMatches, Command, error::ErrorKind, parser::ValueSource};

use crate::config::ChainArg;

/// Base-owned visibility, naming, and rejection rules layered over the derived `base` CLI.
///
/// `base rpc` and `base sequencer` flatten the full execution, consensus, and builder argument
/// sets. Every flag stays parseable, but `-h`/`--help` only lists the curated ones below;
/// `--help-all` lists everything.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CliSurface;

impl CliSurface {
    /// Node subcommands that take `--chain` and accept the renamed flags.
    pub(crate) const NODE_COMMANDS: [&str; 7] =
        ["rpc", "sequencer", "follow", "bootnode", "dev", "execution", "consensus"];

    /// Node subcommands whose help lists only curated flags and rejects [`Self::UNSUPPORTED`].
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
    /// Only command-line use is rejected. Values from env (for example `BASE_NODE_L2_ENGINE_AUTH`
    /// in operator env files shared with the standalone binaries) are ignored.
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

    /// Applies the curated surface to the derived `base` command.
    pub(crate) fn apply(command: Command) -> Command {
        let command = command.mut_arg("base_chain", |arg| arg.hide(true));
        let command = Self::NODE_COMMANDS.into_iter().fold(command, |command, name| {
            command.mut_subcommand(name, |sub| {
                Self::map_args(sub.arg(Self::chain_arg()), Self::rename_arg)
            })
        });
        Self::CURATED_COMMANDS.into_iter().fold(command, |command, name| {
            command.mut_subcommand(name, |sub| Self::curate(sub, Self::command_flags(name)))
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
    pub(crate) fn curate(
        command: Command,
        extra: &'static [(&'static str, &'static str)],
    ) -> Command {
        Self::map_args(command, |arg| Self::curate_arg(arg, extra))
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
    pub(crate) fn curate_arg(arg: Arg, extra: &'static [(&'static str, &'static str)]) -> Arg {
        let Some(long) = arg.get_long().map(str::to_owned) else {
            return arg.hide_short_help(true);
        };
        if Self::UNSUPPORTED.contains(&long.as_str()) {
            return arg.hide(true);
        }
        let Some((_, heading)) =
            Self::NODE_FLAGS.iter().chain(extra).find(|(flag, _)| *flag == long)
        else {
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

    /// Rejects node command lines that pass `--chain` twice or use an [`Self::UNSUPPORTED`] flag.
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
        if !Self::CURATED_COMMANDS.contains(&name) {
            return Ok(());
        }
        let used = sub
            .get_arguments()
            .filter(|arg| arg.get_long().is_some_and(|long| Self::UNSUPPORTED.contains(&long)))
            .find(|arg| from_cli(sub_matches, arg.get_id().as_str()))
            .and_then(Arg::get_long)
            .map(str::to_owned);
        let Some(long) = used else {
            return Ok(());
        };
        Err(sub.error(
            ErrorKind::ArgumentConflict,
            format!(
                "`--{long}` is not supported by `base {name}`: consensus calls the embedded \
                 execution node's Engine API in memory, so Engine API ports, JWTs, and \
                 timeouts do not apply"
            ),
        ))
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
        for flag in CliSurface::UNSUPPORTED {
            assert!(rpc.contains(*flag) && sequencer.contains(*flag), "missing --{flag}");
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
        assert!(help_all.contains("--engine.persistence-threshold"));
        assert!(!help_all.contains("--authrpc.port"));
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
    fn rejects_flags_that_do_not_apply_in_process() {
        for args in [
            &["base", "rpc", "--authrpc.port", "8551"][..],
            &["base", "rpc", "--l2-engine-jwt-secret", "/jwt.hex"],
            &["base", "sequencer", "--disable-auth-server"],
        ] {
            let err = parse(args).unwrap_err();
            assert!(err.to_string().contains("is not supported by `base"), "{err}");
        }
    }

    #[test]
    fn auth_ipc_path_stays_supported() {
        assert!(parse(&["base", "rpc", "--auth-ipc.path", "/data/engine.ipc"]).is_ok());
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
    }
}
