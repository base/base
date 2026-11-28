//! `el live`: the same byte sequences sent to running tiger, base-reth and Nethermind nodes on
//! one devnet chain, with what each did side by side.

mod chain;
mod discv5;
mod peer;
mod rpc;
mod scenarios;

use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
};

use peer::Target;
use rpc::Rpc;

const USAGE: &str = "usage:
  el live setup --dir DIR [--seed S]
  el live chain --dir DIR --node NAME=AUTH_ADDR,HTTP_ADDR... [--blocks N] [--big K]
      (the first --node builds the blocks; every node imports them)
  el live chain --dir DIR --replay --node SOURCE=AUTH,HTTP --node NEW=AUTH,HTTP...
      (copies SOURCE's existing blocks to the other nodes)
  el live rlpx --target NAME=enode://ID@IP:PORT... [--scenario SUBSTRING] [--seed S] [--out FILE]
  el live discv5 --target NAME=ENR_TEXT... [--seed S] [--out FILE]";

fn value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, String> {
    it.next().cloned().ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_node(text: &str, jwt: [u8; 32]) -> Result<(String, Rpc, Rpc), String> {
    let (name, addrs) = text.split_once('=').ok_or("--node NAME=AUTH,HTTP")?;
    let (auth, http) = addrs.split_once(',').ok_or("--node NAME=AUTH,HTTP")?;
    let parse = |a: &str| a.parse::<SocketAddr>().map_err(|e| format!("{a}: {e}"));
    Ok((
        name.to_string(),
        Rpc { addr: parse(auth)?, jwt: Some(jwt) },
        Rpc { addr: parse(http)?, jwt: None },
    ))
}

/// Runs `el live`.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let (command, rest) = args.split_first().map(|(c, r)| (c.as_str(), r)).unwrap_or(("", &[]));
    let mut it = rest.iter();
    let (mut dir, mut seed, mut out, mut filter) = (None, 1u64, None, None);
    let (mut nodes, mut targets) = (Vec::new(), Vec::new());
    let (mut blocks, mut big, mut replay) = (24u64, 20u64, false);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--dir" => dir = Some(PathBuf::from(value(&mut it, flag)?)),
            "--seed" => seed = value(&mut it, flag)?.parse().map_err(|e| format!("--seed: {e}"))?,
            "--out" => out = Some(value(&mut it, flag)?),
            "--scenario" => filter = Some(value(&mut it, flag)?),
            "--node" => nodes.push(value(&mut it, flag)?),
            "--target" => targets.push(value(&mut it, flag)?),
            "--blocks" => {
                blocks = value(&mut it, flag)?.parse().map_err(|e| format!("--blocks: {e}"))?
            }
            "--replay" => replay = true,
            "--big" => big = value(&mut it, flag)?.parse().map_err(|e| format!("--big: {e}"))?,
            other => return Err(format!("unknown flag {other}\n{USAGE}")),
        }
    }
    match command {
        "setup" => chain::setup(&dir.ok_or("--dir")?, seed),
        "chain" => {
            let jwt = chain::jwt(&dir.ok_or("--dir")?)?;
            let nodes: Vec<(String, Rpc, Rpc)> =
                nodes.iter().map(|n| parse_node(n, jwt)).collect::<Result<_, _>>()?;
            let builder = nodes.first().ok_or("--node")?;
            if replay {
                return chain::replay(&builder.2, &nodes[1..]);
            }
            // Blocks are built over the builder's Engine API; reads go to each HTTP port.
            let all: Vec<(String, Rpc, Rpc)> = nodes.clone();
            chain::build(&builder.1, &builder.2, &all, blocks, big)
        }
        "rlpx" => rlpx(&targets, filter.as_deref(), seed, out.as_deref()),
        "discv5" => discv5::run(&targets, seed, out.as_deref()),
        _ => Err(USAGE.to_string()),
    }
}

fn rlpx(
    targets: &[String],
    filter: Option<&str>,
    seed: u64,
    out: Option<&str>,
) -> Result<(), String> {
    let targets: Vec<Target> =
        targets.iter().map(|t| Target::parse(t)).collect::<Result<_, _>>()?;
    let first = targets.first().ok_or("--target")?;
    let ctx = scenarios::read_ctx(first)?;
    println!("chain from {}: {}", first.name, scenarios::describe_ctx(&ctx));
    let mut rows: Vec<(String, String, BTreeMap<String, String>)> = Vec::new();
    for (index, scenario) in scenarios::all().iter().enumerate() {
        if filter.is_some_and(|f| !scenario.name.contains(f) && !scenario.group.contains(f)) {
            continue;
        }
        // Each scenario has its own source address, so a ban by IP stays inside it.
        let source = Ipv4Addr::new(127, 1, (index / 250) as u8, (index % 250 + 2) as u8);
        let mut outcomes = BTreeMap::new();
        for target in &targets {
            let outcome = scenarios::run_one(
                scenario,
                target,
                &ctx,
                crate::el::rng::Rng::case(seed, index as u64).next_u64(),
                source,
            );
            outcomes.insert(target.name.clone(), outcome);
        }
        let differ = outcomes.values().collect::<std::collections::BTreeSet<_>>().len() > 1;
        println!("{} {}/{}", if differ { "DIFF" } else { "same" }, scenario.group, scenario.name);
        for (name, outcome) in &outcomes {
            println!("    {name:<11} {outcome}");
        }
        rows.push((scenario.group.to_string(), scenario.name.to_string(), outcomes));
    }
    let diffs = rows
        .iter()
        .filter(|r| r.2.values().collect::<std::collections::BTreeSet<_>>().len() > 1)
        .count();
    println!("== {} scenarios, {} with differing outcomes ==", rows.len(), diffs);
    if let Some(path) = out {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(g, n, o)| serde_json::json!({ "group": g, "scenario": n, "outcomes": o }))
            .collect();
        std::fs::write(path, serde_json::to_string_pretty(&json).unwrap())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
