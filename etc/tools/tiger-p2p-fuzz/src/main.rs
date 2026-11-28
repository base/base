#![doc = include_str!("../README.md")]

mod el;
mod sim;
mod tp2p;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = args.split_first().map(|(c, r)| (c.as_str(), r)).unwrap_or(("", &[]));
    let result = match command {
        "el" => el::run(rest),
        "sim" => sim::run(rest),
        "tp2p" => tp2p::run(rest),
        _ => Err("usage: base-tiger-p2p-fuzz <el|sim|tp2p> ...".to_string()),
    };
    if let Err(error) = result {
        eprintln!("base-tiger-p2p-fuzz: {error}");
        std::process::exit(1);
    }
}
