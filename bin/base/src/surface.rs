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

    /// Node subcommands that embed an execution node and take [`Self::EXECUTION_RENAMES`].
    /// `base consensus` uses `--port` for its RPC port and `base bootnode` has its own `--nat`.
    pub(crate) const EXECUTION_COMMANDS: [&str; 5] =
        ["rpc", "sequencer", "follow", "dev", "execution"];

    /// Flag prefixes moved into a layer or feature namespace; the rest of the name is unchanged.
    /// The first matching prefix wins.
    pub(crate) const PREFIX_RENAMES: &[(&str, &str)] = &[
        ("p2p.sequencer.", "sequencer."),
        ("p2p.signer.", "sequencer.signer."),
        ("p2p.", "consensus.p2p."),
        ("conductor.", "sequencer.conductor."),
        ("http.", "rpc.http."),
        ("ws.", "rpc.ws."),
        ("discovery.", "execution.p2p.discovery."),
        ("rollup.txpool-", "txpool."),
        ("rollup.mempool-", "txpool."),
        ("validity-", "validity."),
    ];

    /// Flags renamed to Base conventions on every node command. Renamed flags keep their old
    /// spelling as a hidden alias.
    pub(crate) const RENAMES: &[(&str, &str)] = &[
        ("l1-eth-rpc", "l1.rpc"),
        ("l1-beacon", "l1.beacon"),
        ("l1-trust-rpc", "l1.trust-rpc"),
        ("l1-slot-duration-override", "l1.slot-duration-override"),
        ("l1-config-file", "l1.config-file"),
        ("l2-config-file", "l2.config-file"),
        ("execution-chain", "execution.chain"),
        ("enable-tx-forwarding", "tx-forwarding.enabled"),
        ("builder-rpc-urls", "tx-forwarding.builder-urls"),
        ("rollup.payer-enabled", "payer.enabled"),
        ("rollup.payer", "payer.url"),
        ("enable-metering", "metering.enabled"),
        ("enable-shadow-indexer", "shadow-indexer.enabled"),
        ("enable-transaction-event-journal", "transaction-event-journal.enabled"),
        ("builder.enable-resource-metering", "builder.resource-metering.enabled"),
        ("rpc.execution-forwarding-endpoint", "consensus.rpc.execution-forwarding-endpoint"),
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

    /// Execution-layer flags renamed only on [`Self::EXECUTION_COMMANDS`].
    pub(crate) const EXECUTION_RENAMES: &[(&str, &str)] = &[
        ("http", "rpc.http"),
        ("ws", "rpc.ws"),
        ("ipcpath", "rpc.ipc.path"),
        ("port", "execution.p2p.port"),
        ("addr", "execution.p2p.addr"),
        ("nat", "execution.p2p.nat"),
        ("bootnodes", "execution.p2p.bootnodes"),
        ("trusted-peers", "execution.p2p.trusted-peers"),
        ("max-peers", "execution.p2p.max-peers"),
        ("p2p-secret-key", "execution.p2p.secret-key"),
        ("p2p-secret-key-hex", "execution.p2p.secret-key-hex"),
        ("disable-discovery", "execution.p2p.disable-discovery"),
        ("rollup.discovery.v4", "execution.p2p.discovery.v4"),
        ("metrics", "execution.metrics"),
    ];

    /// Root logging and tracing flags renamed to dotted namespaces.
    pub(crate) const ROOT_RENAMES: &[(&str, &str)] = &[
        ("logs-otlp", "logs.otlp"),
        ("logs-otlp.filter", "logs.otlp.filter"),
        ("tracing-otlp", "tracing.otlp"),
        ("tracing-otlp-protocol", "tracing.otlp.protocol"),
        ("tracing-otlp.filter", "tracing.otlp.filter"),
        ("tracing-otlp.service-name", "tracing.otlp.service-name"),
        ("tracing-otlp.sample-ratio", "tracing.otlp.sample-ratio"),
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

    /// Curated help headings, in the order `-h` prints them.
    pub(crate) const HEADINGS: &[&str] = &[
        Self::DEFAULT_HEADING,
        "Storage",
        "JSON-RPC",
        "Consensus RPC",
        "Execution P2P",
        "Consensus P2P",
        "Sequencer",
        "Metrics",
    ];

    /// Curated flags shared by every node command, with their help heading.
    pub(crate) const NODE_FLAGS: &[(&str, &str)] = &[
        ("chain", Self::DEFAULT_HEADING),
        ("l1.rpc", Self::DEFAULT_HEADING),
        ("l1.beacon", Self::DEFAULT_HEADING),
        ("datadir", "Storage"),
        ("full", "Storage"),
        ("rpc.http", "JSON-RPC"),
        ("rpc.http.addr", "JSON-RPC"),
        ("rpc.http.port", "JSON-RPC"),
        ("rpc.http.api", "JSON-RPC"),
        ("rpc.http.corsdomain", "JSON-RPC"),
        ("rpc.ws", "JSON-RPC"),
        ("rpc.ws.addr", "JSON-RPC"),
        ("rpc.ws.port", "JSON-RPC"),
        ("rpc.ws.api", "JSON-RPC"),
        ("rpc.ws.origins", "JSON-RPC"),
        ("consensus.rpc.addr", "Consensus RPC"),
        ("consensus.rpc.port", "Consensus RPC"),
        ("consensus.rpc.enable-admin", "Consensus RPC"),
        ("execution.p2p.addr", "Execution P2P"),
        ("execution.p2p.port", "Execution P2P"),
        ("execution.p2p.discovery.port", "Execution P2P"),
        ("execution.p2p.nat", "Execution P2P"),
        ("execution.p2p.bootnodes", "Execution P2P"),
        ("execution.p2p.trusted-peers", "Execution P2P"),
        ("execution.p2p.max-peers", "Execution P2P"),
        ("execution.p2p.secret-key", "Execution P2P"),
        ("consensus.p2p.listen.ip", "Consensus P2P"),
        ("consensus.p2p.listen.tcp", "Consensus P2P"),
        ("consensus.p2p.listen.udp", "Consensus P2P"),
        ("consensus.p2p.advertise.ip", "Consensus P2P"),
        ("consensus.p2p.bootnodes", "Consensus P2P"),
        ("consensus.p2p.trusted-peers", "Consensus P2P"),
        ("consensus.p2p.priv.path", "Consensus P2P"),
        ("consensus.p2p.peers.hi", "Consensus P2P"),
        ("execution.metrics", "Metrics"),
    ];

    /// Curated flags only on `base sequencer`.
    pub(crate) const SEQUENCER_FLAGS: &[(&str, &str)] = &[
        ("sequencer.stopped", "Sequencer"),
        ("sequencer.recover", "Sequencer"),
        ("sequencer.key.path", "Sequencer"),
        ("sequencer.signer.endpoint", "Sequencer"),
        ("sequencer.signer.address", "Sequencer"),
        ("sequencer.conductor.rpc", "Sequencer"),
    ];

    /// Advanced flags supported on both curated commands, listed only by `--help-all`.
    pub(crate) const ADVANCED_FLAGS: &[&str] = &[
        "execution.chain",
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
        "txpool.ordering",
        "txpool.max-inflight-delegated-slots",
        "txpool.sender-limit",
        "txpool.payer-limit",
        "txpool.allowlisted-payers",
        "txpool.allowlisted-payer-limit",
        "txpool.trusted-delegation-targets",
        "rpc.gascap",
        "rpc.txfeecap",
        "rpc.eth-proof-window",
        "rpc.ipc.path",
        "consensus.rpc.execution-forwarding-endpoint",
        "execution.p2p.secret-key-hex",
        "execution.p2p.disable-discovery",
        "execution.p2p.discovery.v4",
        "consensus.p2p.bootnodes-file",
        "consensus.p2p.scoring",
        "upgrade-signal.contract",
        "upgrade-signal.mode",
        "upgrade-signal.l1-block-tag",
        "upgrade-signal.l1-rpc",
        "payload.resource-metering-schedule",
        "validity.signature-mode",
        "shadow-indexer.enabled",
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
        "tx-forwarding.enabled",
        "tx-forwarding.builder-urls",
        "payer.enabled",
        "payer.url",
        "metering.enabled",
        "metering.metered-opcodes",
        "validity.max-predicates",
        "validity.max-expiry-secs",
        "transaction-event-journal.enabled",
    ];

    /// Advanced flags supported only on `base sequencer`, besides every
    /// [`Self::SEQUENCER_ADVANCED_PREFIX`] flag.
    pub(crate) const SEQUENCER_ADVANCED_FLAGS: &[&str] = &[
        "sequencer.key",
        "sequencer.signer.header",
        "sequencer.signer.tls.ca",
        "sequencer.signer.tls.cert",
        "sequencer.signer.tls.key",
        "sequencer.conductor.rpc.timeout",
        "sequencer.conductor.binary-commit",
        "sequencer.l1-confs",
        "sequencer.l1-rpc-timeout-ms",
        "sequencer.isolated",
        "sequencer.shadow-blocks-per-cycle",
        "sequencer.shadow-funding-address",
        "sequencer.shadow-funding-amount-eth",
        "rollup.sequencer",
        "rollup.sequencer-headers",
    ];

    /// Every builder flag bounds or records block production, so `base sequencer` supports the
    /// whole namespace rather than a list that drifts as builder flags change.
    pub(crate) const SEQUENCER_ADVANCED_PREFIX: &str = "builder.";

    /// Help text replacing upstream text that is ambiguous in a combined node or leaks rustdoc.
    pub(crate) const HELP: &[(&str, &str)] = &[
        ("datadir", "Directory for all node data"),
        ("full", "Run a pruned full node instead of an archive node"),
        ("execution.metrics", "Serve execution Prometheus metrics on this address"),
        ("execution.p2p.port", "Execution P2P (devp2p) TCP listen port"),
        ("execution.p2p.addr", "Execution P2P listen address"),
        ("execution.p2p.discovery.port", "Execution P2P discovery (discv4) UDP port"),
        ("execution.p2p.secret-key", "File holding the execution P2P secret key"),
        ("consensus.rpc.addr", "Consensus RPC (`optimism_*`, `admin_*`) listen address"),
        ("consensus.rpc.port", "Consensus RPC (`optimism_*`, `admin_*`) listen port"),
        ("consensus.rpc.enable-admin", "Enable the consensus `admin_*` RPC namespace"),
        (
            "consensus.p2p.advertise.ip",
            "IP address or DNS hostname advertised to consensus peers. Defaults to \
             `--consensus.p2p.listen.ip`",
        ),
        (
            "sequencer.key.path",
            "File holding the sequencer private key. Conflicts with `--sequencer.key`",
        ),
        (
            "sequencer.signer.endpoint",
            "Remote signer URL. Conflicts with `--sequencer.key` and is required by the other \
             `--sequencer.signer.*` flags",
        ),
        (
            "sequencer.signer.address",
            "Address the remote signer signs for. Requires `--sequencer.signer.endpoint`",
        ),
    ];

    /// Value names that [`Self::value_name`] cannot derive from the flag name.
    pub(crate) const VALUE_NAMES: &[(&str, &str)] = &[
        ("execution.metrics", "IP:PORT"),
        ("execution.p2p.secret-key", "FILE"),
        ("consensus.p2p.bootnodes", "ENRS"),
        ("consensus.p2p.peers.hi", "COUNT"),
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

    /// Returns the position of a curated flag in its command's curated list.
    pub(crate) fn curated_index(name: &str, long: &str) -> Option<usize> {
        Self::NODE_FLAGS.iter().chain(Self::command_flags(name)).position(|(flag, _)| *flag == long)
    }

    /// Returns whether a curated node command supports a flag, by its long name.
    pub(crate) fn supports(name: &str, long: &str) -> bool {
        Self::curated_index(name, long).is_some()
            || Self::ADVANCED_FLAGS
                .iter()
                .chain(Self::command_advanced_flags(name))
                .any(|flag| *flag == long)
            || (name == "sequencer" && long.starts_with(Self::SEQUENCER_ADVANCED_PREFIX))
    }

    /// Returns the Base name for a flag of node command `name`, or `None` if it keeps its name.
    pub(crate) fn renamed(name: &str, long: &str) -> Option<String> {
        let execution: &[(&str, &str)] =
            if Self::EXECUTION_COMMANDS.contains(&name) { Self::EXECUTION_RENAMES } else { &[] };
        Self::RENAMES
            .iter()
            .chain(execution)
            .find(|(old, _)| *old == long)
            .map(|(_, new)| (*new).to_owned())
            .or_else(|| {
                Self::PREFIX_RENAMES
                    .iter()
                    .find_map(|(old, new)| Some(format!("{new}{}", long.strip_prefix(old)?)))
            })
    }

    /// Returns the environment variable derived for a supported flag that has none upstream.
    pub(crate) fn derived_env(long: &str) -> String {
        let env = format!("BASE_{}", long.to_uppercase().replace(['.', '-'], "_"));
        // Kubernetes injects `<SERVICE>_PORT` and `<SERVICE>_SERVICE_PORT` for every Service, so
        // a variable ending in `_PORT` can be set to `tcp://<ip>:<port>` without the operator.
        if env.ends_with("_PORT") { env + "_NUMBER" } else { env }
    }

    /// Sets a value-taking flag's placeholder from its kind of value, such as `<PORT>` or `<URL>`.
    /// Otherwise a one-word upstream placeholder such as `<COUNT>` stays, and a field-derived one
    /// such as `<MEMPOOL_SENDER_LIMIT>` becomes the last segment of the flag name.
    pub(crate) fn tidy_value_name(arg: Arg) -> Arg {
        let Some(long) = arg.get_long().filter(|_| arg.get_action().takes_values()) else {
            return arg;
        };
        let kind = Self::VALUE_NAMES.iter().find(|(flag, _)| *flag == long).map(|(_, name)| *name);
        let kind = kind.or_else(|| match long.rsplit(['.', '-']).next() {
            Some("port" | "tcp" | "udp") => Some("PORT"),
            Some("addr" | "ip") => Some("IP"),
            Some("rpc" | "beacon" | "url" | "urls" | "endpoint") => Some("URL"),
            Some("path" | "file") => Some("FILE"),
            Some("datadir") => Some("DIR"),
            Some("secs") => Some("SECONDS"),
            Some("ms") => Some("MILLISECONDS"),
            _ => None,
        });
        let one_word = |name: &str| name.chars().all(|c| c.is_ascii_uppercase());
        let name = match (kind, arg.get_value_names()) {
            (Some(kind), _) => kind.to_owned(),
            (None, Some([name])) if !one_word(name) => {
                long.rsplit('.').next().unwrap_or(long).to_uppercase().replace('-', "_")
            }
            (None, _) => return arg,
        };
        arg.value_name(name)
    }

    /// Applies the curated surface to the derived `base` command.
    pub(crate) fn apply(command: Command) -> Command {
        let command = command.mut_arg("base_chain", |arg| arg.hide(true));
        let command = Self::map_args(command, |arg| {
            let renamed = arg
                .get_long()
                .and_then(|long| Self::ROOT_RENAMES.iter().find(|(old, _)| *old == long));
            Self::tidy_value_name(match renamed {
                Some((old, new)) => Self::rename(arg, old, new),
                None => arg,
            })
        });
        let command = Self::NODE_COMMANDS.into_iter().fold(command, |command, name| {
            command.mut_subcommand(name, |sub| {
                Self::map_args(sub.arg(Self::chain_arg()), |arg| {
                    let renamed = arg.get_long().and_then(|long| Self::renamed(name, long));
                    match renamed {
                        Some(new) => {
                            let old = arg.get_long().unwrap_or_default().to_owned();
                            Self::rename(arg, &old, &new)
                        }
                        None => arg,
                    }
                })
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
    ///
    /// clap orders help headings by the first argument carrying each heading and cannot reorder
    /// arguments in place, so the command is rebuilt with curated flags first, in
    /// [`Self::HEADINGS`] and table order.
    pub(crate) fn curate(command: Command, name: &'static str) -> Command {
        let command = Self::map_args(command, |arg| Self::curate_arg(arg, name));
        let mut args: Vec<Arg> = command.get_arguments().cloned().collect();
        args.sort_by_key(|arg| {
            let heading = arg.get_help_heading().unwrap_or(Self::DEFAULT_HEADING);
            arg.get_long()
                .and_then(|long| Self::curated_index(name, long))
                .and_then(|index| Some((Self::HEADINGS.iter().position(|h| *h == heading)?, index)))
                .unwrap_or((Self::HEADINGS.len(), 0))
        });
        let mut rebuilt = Command::new(command.get_name().to_owned());
        if let Some(about) = command.get_about() {
            rebuilt = rebuilt.about(about.clone());
        }
        if let Some(long_about) = command.get_long_about() {
            rebuilt = rebuilt.long_about(long_about.clone());
        }
        rebuilt
            .groups(command.get_groups().cloned())
            .args(args.into_iter().enumerate().map(|(order, arg)| arg.display_order(order)))
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

    /// Applies visibility, heading, help, value name, and environment rules to one argument.
    pub(crate) fn curate_arg(arg: Arg, name: &str) -> Arg {
        let Some(long) = arg.get_long().map(str::to_owned) else {
            return arg.hide_short_help(true);
        };
        if !Self::supports(name, &long) {
            return arg.hide(true).env(None::<&str>);
        }
        let mut arg = Self::tidy_value_name(arg);
        if arg.get_env().is_none() {
            arg = arg.env(Self::derived_env(&long));
        }
        let curated = Self::NODE_FLAGS
            .iter()
            .chain(Self::command_flags(name))
            .find(|(flag, _)| *flag == long);
        let Some((_, heading)) = curated else {
            return arg.hide_short_help(true);
        };
        arg = arg.help_heading((*heading != Self::DEFAULT_HEADING).then_some(*heading));
        if let Some((_, help)) = Self::HELP.iter().find(|(flag, _)| *flag == long) {
            arg = arg.help(*help).long_help(None);
        }
        arg
    }

    /// Renames a flag, demoting the old spelling and any upstream aliases to hidden aliases and
    /// pointing examples in its help text at the new spelling.
    pub(crate) fn rename(arg: Arg, old: &str, new: &str) -> Arg {
        let aliases: Vec<String> = arg
            .get_all_aliases()
            .unwrap_or_default()
            .into_iter()
            .filter(|alias| *alias != new)
            .map(str::to_owned)
            .collect();
        let (old_flag, new_flag) = (format!("--{old}"), format!("--{new}"));
        let rewrite = |help: Option<&clap::builder::StyledStr>| {
            help.map(ToString::to_string)
                .filter(|help| help.contains(&old_flag))
                .map(|help| help.replace(&old_flag, &new_flag))
        };
        let (help, long_help) = (rewrite(arg.get_help()), rewrite(arg.get_long_help()));
        let mut arg = aliases
            .into_iter()
            .fold(arg.alias(None).long(new.to_owned()).alias(old.to_owned()), |arg, alias| {
                arg.alias(alias)
            });
        if let Some(help) = help {
            arg = arg.help(help);
        }
        if let Some(long_help) = long_help {
            arg = arg.long_help(long_help);
        }
        arg
    }

    /// Returns the long name a flag of a curated command is known by after renaming.
    pub(crate) fn canonical_long(name: &str, arg: &Arg) -> Option<String> {
        let long = arg.get_long()?;
        Some(Self::renamed(name, long).unwrap_or_else(|| long.to_owned()))
    }

    /// Returns whether a command-line token such as `--flag` or `-f` names `arg` of command `name`.
    pub(crate) fn names(name: &str, arg: &Arg, flag: &str) -> bool {
        let is_long = |long: &str| flag.strip_prefix("--") == Some(long);
        let mut short = flag.chars();
        arg.get_long().is_some_and(is_long)
            || Self::canonical_long(name, arg).is_some_and(|long| is_long(&long))
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
        let unsupported: Vec<(&Arg, String)> = sub
            .get_arguments()
            .filter_map(|arg| Some((arg, Self::canonical_long(name, arg)?)))
            .filter(|(_, long)| !Self::supports(name, long))
            .collect();

        let mut ignored: Vec<IgnoredFlag> = unsupported
            .iter()
            .filter_map(|(arg, long)| {
                let env = arg.get_env()?;
                std::env::var_os(env)?;
                Some(IgnoredFlag {
                    long: long.clone(),
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
            let Some((arg, long)) =
                unsupported.iter().find(|(arg, _)| Self::names(name, arg, flag))
            else {
                kept.push(token.clone());
                continue;
            };
            ignored.push(IgnoredFlag { long: long.clone(), env: None });
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
        for (_, new) in CliSurface::RENAMES.iter().chain(CliSurface::EXECUTION_RENAMES) {
            assert!(
                rpc.contains(*new) || sequencer.contains(*new) || bootnode.contains(*new),
                "missing --{new}"
            );
        }
        for (_, new) in CliSurface::ROOT_RENAMES {
            assert!(command.get_arguments().any(|arg| arg.get_long() == Some(new)), "--{new}");
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
        ])
        .unwrap();
        assert!(cli.ignored.is_empty(), "{:?}", cli.ignored);
    }

    #[test]
    fn builder_and_txpool_safety_flags_reach_the_config() {
        let cli = parse(&[
            "base",
            "sequencer",
            "--builder.max-uncompressed-block-size",
            "1000",
            "--builder.predicate-eval-hard-cutoff-ms",
            "25",
            "--txpool.sender-limit",
            "7",
            "--rollup.mempool-payer-limit",
            "8",
            "--sequencer.isolated",
        ])
        .unwrap();
        assert!(cli.ignored.is_empty(), "{:?}", cli.ignored);
        let BaseCommand::Sequencer(sequencer) = cli.command else {
            panic!("expected sequencer command");
        };
        assert!(sequencer.consensus.sequencer_flags.isolated);
        assert_eq!(sequencer.builder.max_uncompressed_block_size, Some(1000));
        assert_eq!(sequencer.builder.predicate_eval_hard_cutoff_ms, 25);
        assert_eq!(sequencer.builder.rollup_args.mempool_sender_limit, 7);
        assert_eq!(sequencer.builder.rollup_args.mempool_payer_limit, 8);

        let cli = parse(&["base", "rpc", "--txpool.sender-limit", "7"]).unwrap();
        assert!(cli.ignored.is_empty(), "{:?}", cli.ignored);
        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };
        assert_eq!(rpc.execution.standard.rollup_args.mempool_sender_limit, 7);
    }

    #[test]
    fn short_help_lists_headings_in_curated_order() {
        let mut command = curated();
        let help = command.find_subcommand_mut("sequencer").unwrap().render_help().to_string();
        let positions: Vec<usize> = CliSurface::HEADINGS[1..]
            .iter()
            .map(|heading| help.find(&format!("{heading}:")).expect(heading))
            .collect();
        assert!(positions.is_sorted(), "{help}");
        assert!(help.contains("--rpc.http.port <PORT>"), "{help}");
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
            .env("BASE_RPC_HTTP_PORT_NUMBER", "8600")
            .env("BASE_RPC_HTTP_PORT", "tcp://10.0.0.1:8545")
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
        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };
        assert_eq!(rpc.execution.node.rpc.http_port, 8600);
    }
}
