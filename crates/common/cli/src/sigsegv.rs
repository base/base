//! Signal handler to extract a backtrace from stack overflows and segfaults.
//!
//! Uses the [`backtrace`] crate for stack unwinding and symbol resolution,
//! which works on both glibc and musl targets via `libunwind`. This replaces
//! the previous glibc-specific `libc::backtrace` / `backtrace_symbols_fd`
//! approach that was unavailable on musl.
//!
//! Implementation modified from [reth](https://github.com/paradigmxyz/reth/blob/main/crates/cli/util/src/sigsegv_handler.rs#L120).
//!
//! Implementation modified from [`rustc`](https://github.com/rust-lang/rust/blob/3dee9775a8c94e701a08f7b2df2c444f353d8699/compiler/rustc_driver_impl/src/signal_handler.rs).

/// The SIGSEGV handler.
#[derive(Debug, Clone, Copy)]
pub struct SigsegvHandler;

#[cfg(unix)]
impl SigsegvHandler {
    /// Installs a SIGSEGV handler.
    ///
    /// When SIGSEGV is delivered to the process, print a stack trace and then exit.
    pub fn install() {
        unix_impl::install();
    }
}

#[cfg(not(unix))]
impl SigsegvHandler {
    /// No-op on non-unix targets.
    pub const fn install() {}
}

/// All platform-specific implementation lives behind a single `#[cfg(unix)]` gate.
///
/// Uses the [`backtrace`] crate for stack unwinding and symbol resolution,
/// which works on both glibc and musl via `libunwind`. POSIX-standard APIs
/// (`sigaltstack`, `sigaction`) are used for signal handling setup.
/// On Linux, `AT_MINSIGSTKSZ` is read from `/proc/self/auxv` instead of
/// `libc::getauxval`, which is a glibc extension unavailable on musl.
#[cfg(unix)]
mod unix_impl {
    #[cfg(target_os = "linux")]
    use std::io::Read;
    use std::{
        alloc::{Layout, alloc},
        fmt::{Error, Result, Write},
        mem, ptr,
    };

    /// Install the signal handler on the current thread.
    pub(super) fn install() {
        // SAFETY: We allocate a fresh stack for the signal handler and configure
        // sigaction with valid parameters. The signal handler only writes to stderr
        // and does not access any shared mutable state.
        unsafe {
            let alt_stack_size: usize = min_sigstack_size() + 64 * 1024;
            let mut alt_stack: libc::stack_t = mem::zeroed();
            alt_stack.ss_sp = alloc(Layout::from_size_align(alt_stack_size, 1).unwrap()).cast();
            alt_stack.ss_size = alt_stack_size;
            libc::sigaltstack(&alt_stack, ptr::null_mut());

            let mut sa: libc::sigaction = mem::zeroed();
            sa.sa_sigaction = print_stack_trace as *const () as libc::sighandler_t;
            sa.sa_flags = libc::SA_NODEFER | libc::SA_RESETHAND | libc::SA_ONSTACK;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGSEGV, &sa, ptr::null_mut());
        }
    }

    /// Resolve a slice of instruction pointers to symbols and write them to stderr.
    ///
    /// Uses `backtrace::resolve_unsynchronized` which works on both glibc and musl.
    fn backtrace_stderr(buffer: &[*mut libc::c_void], start_index: usize) {
        for (idx, &addr) in buffer.iter().enumerate() {
            let frame = start_index + idx;
            let mut resolved = false;
            // SAFETY: `resolve_unsynchronized` is not strictly async-signal-safe but
            // we are in a crashing signal handler on a dedicated stack. Best-effort
            // diagnostics are acceptable here because the process will terminate
            // immediately after.
            unsafe {
                backtrace::resolve_unsynchronized(addr, |symbol| {
                    resolved = true;
                    let _ = write!(RawStderr, "  {frame:>4}: {addr:?} - ");
                    if let Some(name) = symbol.name() {
                        let _ = write!(RawStderr, "{name}");
                    } else {
                        let _ = write!(RawStderr, "<unknown>");
                    }
                    if let Some(file) = symbol.filename() {
                        let _ = write!(RawStderr, "\n             at {}", file.display());
                        if let Some(line) = symbol.lineno() {
                            let _ = write!(RawStderr, ":{line}");
                        }
                    }
                    let _ = writeln!(RawStderr);
                });
            }
            if !resolved {
                let _ = writeln!(RawStderr, "  {frame:>4}: {addr:?} - <unresolved>");
            }
        }
    }

    /// Unbuffered, unsynchronized writer to stderr.
    ///
    /// Only acceptable because everything will end soon anyways.
    struct RawStderr;

    impl Write for RawStderr {
        fn write_str(&mut self, s: &str) -> Result {
            // SAFETY: libc::write is a standard syscall. STDERR_FILENO is always valid,
            // and we pass a valid pointer and length from the string slice.
            let ret = unsafe { libc::write(libc::STDERR_FILENO, s.as_ptr().cast(), s.len()) };
            if ret == -1 { Err(Error) } else { Ok(()) }
        }
    }

    /// We don't really care how many bytes we actually get out. SIGSEGV comes for our head.
    /// Splash stderr with letters of our own blood to warn our friends about the monster.
    macro_rules! raw_errln {
        ($tokens:tt) => {
            let _ = ::core::fmt::Write::write_fmt(&mut RawStderr, format_args!($tokens));
            let _ = ::core::fmt::Write::write_char(&mut RawStderr, '\n');
        };
    }

    /// Signal handler installed for SIGSEGV.
    extern "C" fn print_stack_trace(_: libc::c_int) {
        const MAX_FRAMES: usize = 256;
        let mut stack_trace: [*mut libc::c_void; MAX_FRAMES] = [ptr::null_mut(); MAX_FRAMES];
        let mut depth = 0usize;

        // SAFETY: `trace_unsynchronized` is the non-thread-safe variant, but we are
        // in a crashing signal handler on a dedicated alt-stack. Best-effort
        // diagnostics are acceptable because the process will terminate shortly.
        // This works on both glibc (via `_Unwind_Backtrace`) and musl (via `libunwind`).
        unsafe {
            backtrace::trace_unsynchronized(|frame| {
                if depth >= MAX_FRAMES {
                    return false;
                }
                stack_trace[depth] = frame.ip();
                depth += 1;
                true
            });
        }

        if depth == 0 {
            return;
        }

        let stack = &stack_trace[..depth];

        // Just a stack trace is cryptic. Explain what we're doing.
        raw_errln!("error: process interrupted by SIGSEGV, printing backtrace\n");
        let mut written = 1;
        let mut consumed = 0;
        // Begin elaborating return addrs into symbols and writing them directly to stderr
        // Most backtraces are stack overflow, most stack overflows are from recursion
        // Check for cycles before writing 250 lines of the same ~5 symbols
        let cycled = |(runner, walker)| runner == walker;
        let mut cyclic = false;
        if let Some(period) = stack.iter().skip(1).step_by(2).zip(stack).position(cycled) {
            let period = period.saturating_add(1); // avoid "what if wrapped?" branches
            let Some(offset) = stack.iter().skip(period).zip(stack).position(cycled) else {
                // impossible.
                return;
            };

            // Count matching trace slices, else we could miscount "biphasic cycles"
            // with the same period + loop entry but a different inner loop
            let next_cycle = stack[offset..].chunks_exact(period).skip(1);
            let cycles = 1 + next_cycle
                .zip(stack[offset..].chunks_exact(period))
                .filter(|(next, prev)| next == prev)
                .count();
            backtrace_stderr(&stack[..offset], consumed);
            written += offset;
            consumed += offset;
            if cycles > 1 {
                raw_errln!("\n### cycle encountered after {offset} frames with period {period}");
                backtrace_stderr(&stack[consumed..consumed + period], consumed);
                raw_errln!("### recursed {cycles} times\n");
                written += period + 4;
                consumed += period * cycles;
                cyclic = true;
            };
        }
        let rem = &stack[consumed..];
        backtrace_stderr(rem, consumed);
        raw_errln!("");
        written += rem.len() + 1;

        let random_depth = || 8 * 16; // chosen by random diceroll (2d20)
        if cyclic || stack.len() > random_depth() {
            // technically speculation, but assert it with confidence anyway.
            // We only arrived in this signal handler because bad things happened
            // and this message is for explaining it's not the programmer's fault
            raw_errln!("note: process unexpectedly overflowed its stack! this is a bug");
            written += 1;
        }
        if stack.len() == MAX_FRAMES {
            raw_errln!("note: maximum backtrace depth reached, frames may have been lost");
            written += 1;
        }
        raw_errln!("note: we would appreciate a bug report at https://github.com/base/base");
        written += 1;
        if written > 24 {
            // We probably just scrolled the earlier "we got SIGSEGV" message off the terminal
            raw_errln!("note: backtrace dumped due to SIGSEGV! resuming signal");
        }
    }

    /// Modern kernels on modern hardware can have dynamic signal stack sizes.
    /// Reads `AT_MINSIGSTKSZ` from `/proc/self/auxv` instead of using
    /// `libc::getauxval`, which is a glibc extension unavailable on musl.
    #[cfg(target_os = "linux")]
    fn min_sigstack_size() -> usize {
        read_at_minsigstksz().unwrap_or(libc::MINSIGSTKSZ)
    }

    /// Read the `AT_MINSIGSTKSZ` value from `/proc/self/auxv`.
    ///
    /// The auxiliary vector is a sequence of `(key, value)` pairs where both
    /// key and value are native-width unsigned integers. The vector is
    /// terminated by an `AT_NULL` (0) entry.
    #[cfg(target_os = "linux")]
    fn read_at_minsigstksz() -> Option<usize> {
        const AT_NULL: usize = 0;
        const AT_MINSIGSTKSZ: usize = 51;
        const ENTRY_SIZE: usize = 2 * std::mem::size_of::<usize>();

        let mut file = std::fs::File::open("/proc/self/auxv").ok()?;
        let mut buf = [0u8; ENTRY_SIZE];
        loop {
            file.read_exact(&mut buf).ok()?;
            let (key_bytes, val_bytes) = buf.split_at(std::mem::size_of::<usize>());
            let key = usize::from_ne_bytes(key_bytes.try_into().ok()?);
            let val = usize::from_ne_bytes(val_bytes.try_into().ok()?);
            if key == AT_NULL {
                return None;
            }
            if key == AT_MINSIGSTKSZ {
                return Some(libc::MINSIGSTKSZ.max(val));
            }
        }
    }

    /// Not all OS support hardware where this is needed.
    #[cfg(not(target_os = "linux"))]
    const fn min_sigstack_size() -> usize {
        libc::MINSIGSTKSZ
    }
}
