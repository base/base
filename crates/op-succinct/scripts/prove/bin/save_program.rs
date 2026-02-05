use std::fs;
use std::path::PathBuf;

use clap::Parser;
use op_succinct_proof_utils::get_range_elf_embedded;

#[derive(Parser)]
#[command(about = "Save the range program ELF to a file")]
struct Args {
    /// Output path for the program binary
    #[arg(short, long, default_value = "program.bin")]
    output: PathBuf,
}

fn main() {
    let args = Args::parse();

    let elf = get_range_elf_embedded();
    fs::write(&args.output, elf).expect("Failed to write program.bin");

    println!("Saved program.bin to {}", args.output.display());
}
