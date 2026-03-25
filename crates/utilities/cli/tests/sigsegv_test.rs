//! Integration tests for the SIGSEGV signal handler.
//!
//! These tests verify that the signal handler installed by [`SigsegvHandler`] catches
//! segfaults (null pointer dereference, stack overflow) and produces meaningful
//! diagnostic output on stderr. Each test spawns a child process that installs the
//! handler and triggers a specific fault, then asserts on the captured stderr output
//! and exit status.
//!
//! The self-re-invocation pattern is used: when the `__SIGSEGV_TEST_MODE` environment
//! variable is set, the test binary enters "crash mode" instead of running assertions.

#![cfg(unix)]

use std::{
    env,
    hint::black_box,
    os::unix::process::ExitStatusExt,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use base_cli_utils::SigsegvHandler;

/// Environment variable used to signal the child process to trigger a fault.
const MODE_ENV: &str = "__SIGSEGV_TEST_MODE";

/// If `__SIGSEGV_TEST_MODE` is set, install the handler and trigger the
/// requested fault. This function never returns in crash mode.
fn dispatch() {
    let Ok(mode) = env::var(MODE_ENV) else {
        return;
    };
    SigsegvHandler::install();

    match mode.as_str() {
        "null_deref" => {
            // SAFETY: intentionally triggering SIGSEGV via null pointer read.
            // `black_box` prevents the compiler from exploiting the UB to
            // optimize away the dereference.
            unsafe { std::ptr::read_volatile(black_box(std::ptr::null::<u8>())) };
            unreachable!();
        }
        "stack_overflow" => {
            // Infinite recursion to overflow the stack and hit the guard page.
            #[inline(never)]
            #[allow(unconditional_recursion)]
            fn recurse(n: u64) -> u64 {
                // Allocate stack space to accelerate overflow and prevent tail-call
                // optimization.
                let buf = [n; 64];
                recurse(black_box(buf[0].wrapping_add(1)))
            }
            let _ = recurse(black_box(0));
            unreachable!();
        }
        other => {
            eprintln!("unknown test mode: {other}");
            std::process::exit(2);
        }
    }
}

/// Maximum time to wait for the child process before killing it.
///
/// Generous enough for backtrace resolution under debug builds, but prevents
/// CI from hanging indefinitely if the backtrace resolver deadlocks (e.g.
/// musl allocator contention inside `dl_iterate_phdr` during symbol resolution).
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawn the current test binary as a child process in crash mode.
///
/// Re-invokes the current executable targeting `test_name` with `mode` set
/// in the environment. Returns the child's captured output. Panics if the
/// child does not exit within [`CHILD_TIMEOUT`].
fn run_child(test_name: &str, mode: &str) -> Output {
    let mut child = Command::new(env::current_exe().expect("current_exe"))
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(MODE_ENV, mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn child process");

    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().expect("failed to collect output"),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!(
                        "child process for test '{test_name}' timed out after {CHILD_TIMEOUT:?}"
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("failed to wait on child process: {e}"),
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Assert the child was killed by a signal and the SIGSEGV banner was printed.
fn assert_killed_by_sigsegv(output: &Output, stderr: &str) {
    assert!(!output.status.success(), "child should have been terminated by a signal, got success");
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGSEGV),
        "child should have been killed by SIGSEGV, status: {}",
        output.status,
    );
    assert!(
        stderr.contains("process interrupted by SIGSEGV"),
        "stderr should contain SIGSEGV banner.\nstderr:\n{stderr}"
    );
}

// ── Tests ──────────────────────────────────────────────────────────────

#[test]
fn segfault_produces_backtrace() {
    dispatch();

    let output = run_child("segfault_produces_backtrace", "null_deref");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_killed_by_sigsegv(&output, &stderr);

    assert!(
        stderr.contains("0x"),
        "stderr should contain at least one resolved address.\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("we would appreciate a bug report"),
        "stderr should contain bug report notice.\nstderr:\n{stderr}"
    );
}

/// On macOS, stack overflows are caught by the Rust runtime via Mach exception ports,
/// which preempt POSIX signal handlers. The SIGSEGV handler's cycle detection logic
/// is only exercised on Linux with glibc, where stack guard page faults deliver
/// SIGSEGV through the standard signal path and `_Unwind_Backtrace` can walk back
/// through the overflowed stack frames.
///
/// Under musl, `libunwind` cannot unwind past the signal trampoline into the
/// overflowed stack, so the backtrace is too shallow for cycle detection.
/// See [`stack_overflow_handled_on_musl`] for the musl-specific variant.
#[test]
#[cfg(all(target_os = "linux", not(target_env = "musl")))]
fn stack_overflow_detects_cycle() {
    dispatch();

    let output = run_child("stack_overflow_detects_cycle", "stack_overflow");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_killed_by_sigsegv(&output, &stderr);

    // The handler only prints cycle info when it detects more than one repeated
    // cycle in the captured frames (up to 256). With a ~1-3 frame cycle period
    // and thousands of recursion frames, cycle count should be well above 1.
    let has_cycle_msg = stderr.contains("cycle encountered") || stderr.contains("recursed");
    assert!(has_cycle_msg, "stderr should contain cycle detection output.\nstderr:\n{stderr}");
    assert!(
        stderr.contains("unexpectedly overflowed its stack"),
        "stderr should contain stack overflow note.\nstderr:\n{stderr}"
    );
}

/// Under musl, `libunwind` cannot unwind through the corrupted stack after a guard
/// page fault, so the captured backtrace is too shallow for cycle detection. This
/// test verifies the handler still catches the signal, runs on the alternate stack,
/// and produces the SIGSEGV diagnostic banner.
#[test]
#[cfg(all(target_os = "linux", target_env = "musl"))]
fn stack_overflow_handled_on_musl() {
    dispatch();

    let output = run_child("stack_overflow_handled_on_musl", "stack_overflow");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_killed_by_sigsegv(&output, &stderr);

    assert!(
        stderr.contains("we would appreciate a bug report"),
        "stderr should contain bug report notice.\nstderr:\n{stderr}"
    );
}
