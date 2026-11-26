//! Bounded external operations, separate from the UI and PDFium worker.
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;
pub use synctex_parser::{NAVIGATION_TIMEOUT, Operation};

const OUTPUT_LIMIT: usize = 1024 * 1024;

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
        if out_done && err_done {
            std::thread::sleep(Duration::from_millis(2));
        } else {
            let timeout = operation.remaining()?.min(Duration::from_millis(20));
            let mut fds = [
                libc::pollfd {
                    fd: if out_done { -1 } else { stdout.as_raw_fd() },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if err_done { -1 } else { stderr.as_raw_fd() },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout.as_millis().max(1) as i32) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
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
