//! Bundles a raw RISC-V ELF into the R0BF (RISC Zero Binary Format) and
//! computes the image ID.
//!
//! Usage:
//!     cargo run --manifest-path tools/compute-image-id/Cargo.toml -- <elf-path> [--output <r0bf-path>]

use std::{env, fs, process};

fn main() {
    let args: Vec<String> = env::args().collect();
    let (elf_path, output_path) = parse_args(&args);

    let elf_bytes = fs::read(&elf_path).unwrap_or_else(|e| {
        eprintln!("error: failed to read {elf_path}: {e}");
        process::exit(1);
    });

    if elf_bytes.starts_with(b"R0BF") {
        // Already bundled — just compute the image ID.
        let image_id = risc0_binfmt::compute_image_id(&elf_bytes).unwrap_or_else(|e| {
            eprintln!("error: failed to compute image ID from R0BF: {e}");
            process::exit(1);
        });
        println!("Image ID: {image_id}");
        return;
    }

    if !elf_bytes.starts_with(b"\x7fELF") {
        eprintln!("error: {elf_path} is neither a raw ELF nor R0BF format");
        process::exit(1);
    }

    let kernel_elf = risc0_zkos_v1compat::V1COMPAT_ELF;
    let bundled = risc0_binfmt::ProgramBinary::new(&elf_bytes, kernel_elf);

    let image_id = bundled.compute_image_id().unwrap_or_else(|e| {
        eprintln!("error: failed to compute image ID: {e}");
        process::exit(1);
    });
    println!("Image ID: {image_id}");

    if let Some(out) = output_path {
        let encoded = bundled.encode();
        fs::write(&out, &encoded).unwrap_or_else(|e| {
            eprintln!("error: failed to write {out}: {e}");
            process::exit(1);
        });
        println!("Bundled R0BF written to: {out} ({} bytes)", encoded.len());
    }
}

fn parse_args(args: &[String]) -> (String, Option<String>) {
    let mut elf_path = None;
    let mut output_path = None;
    let mut i = 1;

    while i < args.len() {
        if args[i] == "--output" || args[i] == "-o" {
            i += 1;
            if i >= args.len() {
                eprintln!("error: --output requires a path argument");
                process::exit(1);
            }
            output_path = Some(args[i].clone());
        } else if elf_path.is_none() {
            elf_path = Some(args[i].clone());
        } else {
            eprintln!("error: unexpected argument: {}", args[i]);
            process::exit(1);
        }
        i += 1;
    }

    let elf_path = elf_path.unwrap_or_else(|| {
        eprintln!("usage: compute-image-id <elf-path> [--output <r0bf-path>]");
        process::exit(1);
    });

    (elf_path, output_path)
}
