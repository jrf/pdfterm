//! Bounded external operations, separate from the UI and PDFium worker.
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(10);
const OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Clone)]
pub struct Operation {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}
impl Operation {
    pub fn new(timeout: Duration) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + timeout,
        }
    }
    /// Keep the parent's cancellation while applying a shorter helper deadline.
    pub fn limited(&self, timeout: Duration) -> Self {
        Self {
            cancelled: Arc::clone(&self.cancelled),
            deadline: self.deadline.min(Instant::now() + timeout),
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub fn remaining(&self) -> io::Result<Duration> {
        self.check()?;
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub fn check(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "navigation cancelled",
            ))
        } else if Instant::now() >= self.deadline {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "navigation timed out",
            ))
        } else {
            Ok(())
        }
    }
}
impl Default for Operation {
    fn default() -> Self {
        Self::new(NAVIGATION_TIMEOUT)
    }
}

struct ChildGroup(Child);
impl Drop for ChildGroup {
    fn drop(&mut self) {
        // The child owns a new process group. Kill descendants too, including a
        // helper that outlives its parent while holding an output pipe open.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}
fn nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
    let fd = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn drain(reader: &mut impl Read, output: &mut Vec<u8>) -> io::Result<bool> {
    // Bound each poll so an endlessly writing helper cannot starve cancellation.
    let mut buffer = [0; 8192];
    for _ in 0..16 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if output.len() + n > OUTPUT_LIMIT {
                    return Err(io::Error::other("helper output exceeds 1 MiB"));
                }
                output.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}
pub fn output(command: &mut Command, operation: &Operation) -> io::Result<Output> {
    operation.check()?;
    let mut child = ChildGroup(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?,
    );
    let mut stdout = child.0.stdout.take().expect("piped stdout");
    let mut stderr = child.0.stderr.take().expect("piped stderr");
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    loop {
        operation.check()?;
        let out_done = drain(&mut stdout, &mut out)?;
        let err_done = drain(&mut stderr, &mut err)?;
        if let Some(status) = child.0.try_wait()?
            && out_done
            && err_done
        {
            return Ok(Output {
                status,
                stdout: out,
                stderr: err,
            });
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stalled_and_noisy_helpers_are_bounded() {
        let started = Instant::now();
        let error = output(
            Command::new("/bin/sh").args(["-c", "sleep 30 & wait"]),
            &Operation::new(Duration::from_millis(50)),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            output(
                Command::new("/bin/sh").args(["-c", "yes flood"]),
                &Operation::default()
            )
            .unwrap_err()
            .to_string()
            .contains("1 MiB")
        );
    }
    #[test]
    fn cancellation_interrupts_active_child() {
        let operation = Operation::default();
        let control = operation.clone();
        let thread = std::thread::spawn(move || {
            output(Command::new("/bin/sh").args(["-c", "sleep 30"]), &operation)
        });
        std::thread::sleep(Duration::from_millis(30));
        control.cancel();
        assert_eq!(
            thread.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }
}
