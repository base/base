//! `el` subcommand: differential fuzzing of tiger's devp2p stack against base-reth (in-process,
//! `el decode`) and against running base-reth and Nethermind nodes (`el live`).

mod chain;
mod decode;
mod discv5;
mod ecies;
mod enr;
mod eth;
mod live;
mod p2p;
mod rlp;
mod rng;
mod snappy;
mod status;
mod wire;

use decode::{Target, default_mutate};

const USAGE: &str = "usage:
  base-tiger-p2p-fuzz el decode [--seed S] [--iters N] [--target NAME] [--out FILE]
      targets: snappy, p2p, eth, status, forkid, ecies, enr, discv5 (default: all)
  base-tiger-p2p-fuzz el live ... (see README)";

/// Parsed `el decode` options.
#[derive(Debug, Clone)]
pub(crate) struct Args {
    seed: u64,
    iters: u64,
    target: Option<String>,
    out: Option<String>,
}

impl Args {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut out = Self { seed: 1, iters: 2_000, target: None, out: None };
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let mut value = || it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--seed" => out.seed = value()?.parse().map_err(|e| format!("--seed: {e}"))?,
                "--iters" => out.iters = value()?.parse().map_err(|e| format!("--iters: {e}"))?,
                "--target" => out.target = Some(value()?),
                "--out" => out.out = Some(value()?),
                other => return Err(format!("unknown flag {other}\n{USAGE}")),
            }
        }
        Ok(out)
    }
}

/// Every decode target.
fn targets() -> Vec<Target> {
    vec![
        Target {
            name: "snappy",
            generate: snappy::generate,
            mutate: snappy::mutate,
            check: snappy::check,
            minimize: true,
        },
        Target {
            name: "p2p",
            generate: p2p::generate,
            mutate: default_mutate,
            check: p2p::check,
            minimize: true,
        },
        Target {
            name: "eth",
            generate: eth::generate,
            mutate: eth::mutate,
            check: eth::check,
            minimize: true,
        },
        Target {
            name: "status",
            generate: status::generate_status,
            mutate: default_mutate,
            check: status::check_status,
            minimize: true,
        },
        Target {
            name: "forkid",
            generate: status::generate_forkid,
            mutate: default_mutate,
            check: status::check_forkid,
            minimize: true,
        },
        Target {
            name: "ecies",
            generate: ecies::generate,
            mutate: ecies::mutate,
            check: ecies::check,
            minimize: true,
        },
        Target {
            name: "enr",
            generate: enr::generate,
            mutate: enr::mutate,
            check: enr::check,
            minimize: true,
        },
        // tiger's side answers from a live socket and rate-limits challenges per source, so
        // replaying one input while shrinking it is not meaningful.
        Target {
            name: "discv5",
            generate: discv5::generate,
            mutate: discv5::mutate,
            check: discv5::check,
            minimize: false,
        },
    ]
}

/// Runs the `el` subcommand.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let (command, rest) = args.split_first().map(|(c, r)| (c.as_str(), r)).unwrap_or(("", &[]));
    match command {
        "decode" => decode::run(&Args::parse(rest)?, &targets()),
        "live" => live::run(rest),
        _ => Err(USAGE.to_string()),
    }
}
