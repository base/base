#![allow(missing_docs)]

fn main() {
    println!("cargo:rerun-if-changed=cpp/lib.cpp");

    let cxxflags = ["llvm-config", "llvm-config-22"]
        .into_iter()
        .find_map(|llvm_config| {
            match std::process::Command::new(llvm_config).arg("--cxxflags").output() {
                Ok(output) => Some(String::from_utf8(output.stdout).unwrap()),
                Err(e) => {
                    eprintln!("failed to run {llvm_config}: {e}");
                    None
                }
            }
        })
        .expect("no llvm-config found");

    cc::Build::new()
        .cpp(true)
        .flags(cxxflags.split_whitespace())
        .flag("-w")
        .file("cpp/lib.cpp")
        .compile("revmc_llvm_cpp");
}
