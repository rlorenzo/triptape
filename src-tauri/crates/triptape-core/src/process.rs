//! Child processes that a job's cancel can stop.
//!
//! Renders spawn FFmpeg and Typst from deep inside the media crate. Rather than
//! threading a flag through every signature, a job installs its [`CancelFlag`]
//! for the current thread with [`with_cancel`], and [`output`] and [`status`]
//! kill their child as soon as the flag is set. With no flag installed they
//! behave exactly like the `Command` methods they replace.

use crate::jobs::CancelFlag;
use std::cell::RefCell;
use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::Duration;

thread_local! {
    static CURRENT: RefCell<Option<CancelFlag>> = const { RefCell::new(None) };
}

/// Longest wait between checks on a running child, and so the longest a
/// cancel takes to reach it.
const MAX_POLL: Duration = Duration::from_millis(50);

/// Run `f` with `flag` installed on this thread, restoring the previous flag
/// afterward. Threads spawned inside `f` must install it themselves; see
/// [`current`].
pub fn with_cancel<R>(flag: &CancelFlag, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<CancelFlag>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CURRENT.with(|c| c.borrow_mut().replace(flag.clone())));
    f()
}

/// The flag installed on this thread, if any.
pub fn current() -> Option<CancelFlag> {
    CURRENT.with(|c| c.borrow().clone())
}

/// True when this thread's job has been cancelled.
pub fn cancelled() -> bool {
    current().is_some_and(|f| f.is_cancelled())
}

fn cancelled_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

/// Wait for `child`, killing it if `flag` is set first.
fn wait_or_kill(child: &mut Child, flag: &CancelFlag) -> io::Result<ExitStatus> {
    let mut poll = Duration::from_millis(1);
    loop {
        if flag.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(cancelled_error());
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        std::thread::sleep(poll);
        poll = (poll * 2).min(MAX_POLL);
    }
}

/// [`Command::status`] that a cancel can interrupt.
pub fn status(cmd: &mut Command) -> io::Result<ExitStatus> {
    let Some(flag) = current() else {
        return cmd.status();
    };
    if flag.is_cancelled() {
        return Err(cancelled_error());
    }
    wait_or_kill(&mut cmd.spawn()?, &flag)
}

/// [`Command::output`] that a cancel can interrupt.
pub fn output(cmd: &mut Command) -> io::Result<Output> {
    let Some(flag) = current() else {
        return cmd.output();
    };
    if flag.is_cancelled() {
        return Err(cancelled_error());
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Both pipes drain on their own threads: a child that fills one would
    // otherwise block forever and never exit.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = vec![];
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let status = wait_or_kill(&mut child, &flag);
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok(Output {
        status: status?,
        stdout,
        stderr,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn without_a_flag_commands_run_normally() {
        let out = output(&mut sh("echo hi")).unwrap();
        assert_eq!(out.stdout, b"hi\n");
        assert!(status(&mut sh("exit 3")).unwrap().code() == Some(3));
    }

    #[test]
    fn with_a_flag_output_is_still_captured() {
        let flag = CancelFlag::new();
        let out = with_cancel(&flag, || {
            // Larger than a pipe buffer, on both streams.
            output(&mut sh(
                "head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2; exit 2",
            ))
        })
        .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(out.stdout.len(), 200_000);
        assert_eq!(out.stderr.len(), 200_000);
    }

    #[test]
    fn cancel_kills_a_running_child_quickly() {
        let flag = CancelFlag::new();
        let canceller = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            canceller.cancel();
        });
        let start = Instant::now();
        // `exec` so the shell itself is the sleeping child holding the pipes.
        let err = with_cancel(&flag, || output(&mut sh("exec sleep 30"))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_cancelled_job_starts_nothing() {
        let flag = CancelFlag::new();
        flag.cancel();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let script = format!("touch '{}'", marker.display());
        with_cancel(&flag, || {
            assert!(output(&mut sh(&script)).is_err());
            assert!(status(&mut sh(&script)).is_err());
        });
        assert!(!marker.exists());
    }

    #[test]
    fn the_flag_is_scoped_to_the_closure() {
        let flag = CancelFlag::new();
        flag.cancel();
        with_cancel(&flag, || assert!(cancelled()));
        assert!(!cancelled());
    }
}
