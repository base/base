#![doc = include_str!("../README.md")]

pub use imp::ChildExt;

#[cfg(unix)]
mod imp {
    use std::{
        io,
        process::{Child, ExitStatus},
        time::{Duration, Instant},
    };

    /// Extension trait for waiting on a child process with a timeout.
    pub trait ChildExt {
        /// Wait for the child to exit, returning `None` if the timeout elapses first.
        fn wait_timeout(&mut self, dur: Duration) -> io::Result<Option<ExitStatus>>;
    }

    impl ChildExt for Child {
        fn wait_timeout(&mut self, dur: Duration) -> io::Result<Option<ExitStatus>> {
            use std::os::unix::process::ExitStatusExt;
            let deadline = Instant::now() + dur;
            let pid = self.id() as libc::pid_t;
            loop {
                let mut status: libc::c_int = 0;
                let ret = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                if ret < 0 {
                    return Err(io::Error::last_os_error());
                } else if ret > 0 {
                    return Ok(Some(ExitStatus::from_raw(status)));
                }
                let now = Instant::now();
                if now >= deadline {
                    return Ok(None);
                }
                let remaining = deadline - now;
                std::thread::sleep(remaining.min(Duration::from_millis(1)));
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::{
        io,
        process::{Child, ExitStatus},
        time::Duration,
    };

    /// Extension trait for waiting on a child process with a timeout.
    pub trait ChildExt {
        /// Wait for the child to exit, returning `None` if the timeout elapses first.
        fn wait_timeout(&mut self, dur: Duration) -> io::Result<Option<ExitStatus>>;
    }

    impl ChildExt for Child {
        fn wait_timeout(&mut self, _dur: Duration) -> io::Result<Option<ExitStatus>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "wait_timeout not supported on this platform",
            ))
        }
    }
}
