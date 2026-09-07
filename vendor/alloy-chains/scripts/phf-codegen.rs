#!/usr/bin/env cargo -Zscript
---
[package]
edition = "2024"

[dependencies]
phf_codegen = "0.14"
---

use std::{
    fmt::Write as _,
    io::{self, Read},
};

struct MapSpec {
    name: String,
    cfg: Option<String>,
    key: MapKey,
    entries: Vec<(String, String)>,
}

#[derive(Clone, Copy)]
enum MapKey {
    Str,
    U64,
}

fn main() {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).expect("can read stdin");

    let maps = parse_maps(&input);
    let mut output = String::new();
    for map in maps {
        if let Some(cfg) = map.cfg {
            writeln!(output, "{cfg}").unwrap();
        }

        match map.key {
            MapKey::Str => {
                let mut builder = phf_codegen::Map::new();
                for (key, value) in &map.entries {
                    builder.entry(key.as_str(), value.as_str());
                }

                writeln!(
                    output,
                    "static {}: phf::Map<&'static str, ChainIndex> = {};",
                    map.name,
                    builder.build()
                )
                .unwrap();
            }
            MapKey::U64 => {
                let mut builder = phf_codegen::Map::new();
                for (key, value) in &map.entries {
                    let key = key.parse::<u64>().expect("u64 map key");
                    builder.entry(key, value.as_str());
                }

                writeln!(
                    output,
                    "static {}: phf::Map<u64, ChainIndex> = {};",
                    map.name,
                    builder.build()
                )
                .unwrap();
            }
        }
    }

    print!("{output}");
}

fn parse_maps(input: &str) -> Vec<MapSpec> {
    let mut maps = Vec::new();
    let mut current: Option<MapSpec> = None;

    for (line_no, line) in input.lines().enumerate() {
        let line_no = line_no + 1;
        if line.is_empty() {
            continue;
        }

        if line == "end" {
            maps.push(
                current.take().unwrap_or_else(|| panic!("line {line_no}: `end` without map")),
            );
            continue;
        }

        if let Some(rest) = line.strip_prefix("map\t") {
            if current.is_some() {
                panic!("line {line_no}: nested map");
            }
            let mut parts = rest.splitn(2, '\t');
            let name = parts.next().expect("name").to_owned();
            let cfg = parts.next().filter(|cfg| !cfg.is_empty()).map(str::to_owned);
            current = Some(MapSpec { name, cfg, key: MapKey::Str, entries: Vec::new() });
            continue;
        }

        if let Some(rest) = line.strip_prefix("map_u64\t") {
            if current.is_some() {
                panic!("line {line_no}: nested map");
            }
            let mut parts = rest.splitn(2, '\t');
            let name = parts.next().expect("name").to_owned();
            let cfg = parts.next().filter(|cfg| !cfg.is_empty()).map(str::to_owned);
            current = Some(MapSpec { name, cfg, key: MapKey::U64, entries: Vec::new() });
            continue;
        }

        let current =
            current.as_mut().unwrap_or_else(|| panic!("line {line_no}: entry without map"));
        let Some((key, value)) = line.split_once('\t') else {
            panic!("line {line_no}: expected tab-separated key and value");
        };
        current.entries.push((key.to_owned(), value.to_owned()));
    }

    if current.is_some() {
        panic!("unterminated map");
    }

    maps
}
