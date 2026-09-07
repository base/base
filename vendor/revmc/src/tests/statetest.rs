use std::path::PathBuf;

use revmc_statetest::compiled::CompileMode;

fn is_ci() -> bool {
    std::env::var_os("CI").is_some()
}

fn collect_tests(path_fn: fn() -> Option<PathBuf>, label: &str) -> Vec<PathBuf> {
    let Some(path) = path_fn() else {
        eprintln!("Skipping {label}: not available");
        return Vec::new();
    };
    revmc_statetest::find_all_json_tests(&path)
}

fn run_state_tests(mode: CompileMode) {
    let mut test_files = Vec::new();

    // Legacy submodule (ethereum/tests tarball).
    if let Some(mut path) = revmc_statetest::get_general_state_tests_path() {
        let mut subdir = std::env::var("SUBDIR").ok();
        if matches!(mode, CompileMode::Aot) && subdir.is_none() && !is_ci() {
            subdir = Some("stRevertTest".into());
        }
        if let Some(subdir) = subdir {
            path = path.join(subdir);
        }
        test_files.extend(revmc_statetest::find_all_json_tests(&path));
    } else {
        eprintln!(
            "Skipping legacy submodule tests: not available \
             (run `git submodule update --init --checkout --depth 1 tests/ethereum-tests`)"
        );
    }

    // Additional test suites from ./scripts/setup-test-fixtures.sh (CI only, interpreter/JIT).
    if is_ci() && !matches!(mode, CompileMode::Aot) {
        test_files.extend(collect_tests(
            revmc_statetest::get_exec_spec_stable_state_tests_path,
            "exec-spec-tests stable",
        ));
        test_files.extend(collect_tests(
            revmc_statetest::get_exec_spec_develop_state_tests_path,
            "exec-spec-tests develop",
        ));
        test_files.extend(collect_tests(
            revmc_statetest::get_legacy_cancun_state_tests_path,
            "legacy Cancun",
        ));
        test_files.extend(collect_tests(
            revmc_statetest::get_legacy_constantinople_state_tests_path,
            "legacy Constantinople",
        ));
    }

    if test_files.is_empty() {
        eprintln!("No test files found");
        return;
    }

    let single_thread = std::env::var("SINGLE_THREAD").is_ok();
    revmc_statetest::compiled::run(test_files, single_thread, false, mode).unwrap();
}

#[test]
fn interpreter() {
    run_state_tests(CompileMode::Interpreter);
}

#[test]
fn jit() {
    run_state_tests(CompileMode::Jit);
}

#[test]
fn aot() {
    run_state_tests(CompileMode::Aot);
}

// ── Blockchain tests (execution-spec-tests) ──

fn run_blockchain_tests() {
    if !is_ci() {
        eprintln!("Skipping blockchain tests outside CI");
        return;
    }

    let mut test_files = Vec::new();

    test_files.extend(collect_tests(
        revmc_statetest::get_exec_spec_stable_blockchain_tests_path,
        "exec-spec-tests stable blockchain_tests",
    ));
    test_files.extend(collect_tests(
        revmc_statetest::get_exec_spec_develop_blockchain_tests_path,
        "exec-spec-tests develop blockchain_tests",
    ));

    if test_files.is_empty() {
        eprintln!("No blockchain test files found");
        return;
    }

    revmc_statetest::btest::run_btests(test_files).unwrap();
}

#[test]
fn blockchain() {
    run_blockchain_tests();
}
