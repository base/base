#![allow(missing_docs)]

fn main() {
    revmc_build::emit();

    // Detect `-Cforce-frame-pointers=yes` in rustflags and expose as a cfg.
    println!("cargo:rustc-check-cfg=cfg(force_frame_pointers)");
    if let Some(flags) = std::env::var_os("CARGO_ENCODED_RUSTFLAGS") {
        let flags = flags.to_string_lossy();
        for flag in flags.split('\x1f') {
            if matches!(flag, "force-frame-pointers=yes" | "force-frame-pointers=y") {
                println!("cargo:rustc-cfg=force_frame_pointers");
                break;
            }
            if let Some(rest) = flag.strip_prefix("-C") {
                let rest = rest.trim_start();
                if matches!(
                    rest,
                    "force-frame-pointers=yes" | "force-frame-pointers=y" | "force-frame-pointers"
                ) {
                    println!("cargo:rustc-cfg=force_frame_pointers");
                    break;
                }
            }
        }
    }
}
