use crate::synctex::{ForwardRequest, parse_forward_request};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Cancellable connection, including a saturated Unix listener backlog.
pub(crate) fn connect(path: &str, operation: &crate::process::Operation) -> io::Result<UnixStream> {
    use std::os::fd::FromRawFd;
    operation.check()?;
    // Zero initialization supplies the trailing NUL and optional platform fields.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_bytes();
    if bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::other("invalid Unix socket path"));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as _;
    }
    #[cfg(target_os = "macos")]
    {
        address.sun_len = std::mem::size_of_val(&address) as u8;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    stream.set_nonblocking(true)?;
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    loop {
        operation.check()?;
        let result = unsafe {
            libc::connect(
                fd,
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&address) as _,
            )
        };
        if result == 0 {
            operation.check()?;
            return Ok(stream);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => {
                operation.check()?;
                return Ok(stream);
            }
            Some(libc::EINPROGRESS | libc::EALREADY) => {
                wait_socket(&stream, libc::POLLOUT, operation)?;
                if let Some(error) = stream.take_error()? {
                    return Err(error);
                }
                operation.check()?;
                return Ok(stream);
            }
            // Linux does not queue an AF_UNIX connect when its backlog is full.
            // An unconnected socket can poll writable, so retry connect itself.
            Some(libc::EAGAIN) => {
                std::thread::sleep(operation.remaining()?.min(Duration::from_millis(2)));
            }
            Some(libc::EINTR) => {}
            _ => return Err(error),
        }
    }
}

fn wait_socket(
    stream: &UnixStream,
    events: libc::c_short,
    operation: &crate::process::Operation,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        let timeout = operation.remaining()?.min(Duration::from_millis(50));
        let mut descriptor = libc::pollfd {
            fd: stream.as_raw_fd(),
            events,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout.as_millis().max(1) as i32) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        operation.check()?;
        if ready > 0 {
            return Ok(());
        }
    }
}

pub(crate) fn send(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    operation: &crate::process::Operation,
) -> io::Result<()> {
    while !bytes.is_empty() {
        operation.check()?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait_socket(stream, libc::POLLOUT, operation)?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    loop {
        operation.check()?;
        match stream.shutdown(Shutdown::Write) {
            Ok(()) => return operation.check(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}
/// Only the current user may traverse directories containing IPC or native code.
pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: geteuid takes no arguments and has no failure mode.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be a real directory owned by the current user with mode 0700",
                path.display()
            ),
        ));
    }
    Ok(())
}

pub(crate) struct Listener {
    pub socket: UnixListener,
    path: PathBuf,
    identity: (u64, u64),
}

impl Listener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        private_dir(
            path.parent()
                .ok_or_else(|| io::Error::other("socket path has no parent"))?,
        )?;
        // Never unlink a live listener, symlink, or unrelated file. A crash leaves
        // a stale socket that its owner must explicitly remove before restarting.
        let socket = UnixListener::bind(path).map_err(|error| {
            io::Error::new(error.kind(), format!("cannot bind {}: {error}; an existing socket must be stopped or explicitly removed", path.display()))
        })?;
        let metadata = fs::symlink_metadata(path)?;
        let listener = Self {
            socket,
            path: path.to_owned(),
            identity: (metadata.dev(), metadata.ino()),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        listener.socket.set_nonblocking(true)?;
        Ok(listener)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && (metadata.dev(), metadata.ino()) == self.identity
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    pub ok: bool,
    pub error: Option<String>,
    #[serde(rename = "viewer_token", default)]
    _viewer_token: Option<String>,
}

#[derive(Serialize)]
struct ReplyBody<'a> {
    ok: bool,
    error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    viewer_token: Option<&'a str>,
}

#[derive(Debug)]
pub enum ViewerRequest {
    Forward(ForwardRequest),
    Screenshot(PathBuf),
    Focus(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FocusRequest {
    #[serde(rename = "type")]
    kind: String,
    viewer_token: String,
}

fn parse_focus_request(payload: &str) -> io::Result<String> {
    let request: FocusRequest = serde_json::from_str(payload)?;
    if request.kind != "focus"
        || request.viewer_token.len() != 32
        || !request
            .viewer_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid focus request",
        ));
    }
    Ok(request.viewer_token)
}
pub(crate) const FORWARD_TIMEOUT: Duration = Duration::from_secs(30);

/// One terminal reply per connection, including unwinding/normal viewer shutdown.
pub struct ForwardReply(Option<UnixStream>);

impl ForwardReply {
    pub fn new(stream: UnixStream) -> Self {
        Self(Some(stream))
    }

    pub fn disconnected(&self) -> io::Result<bool> {
        use std::os::fd::AsRawFd;
        let Some(stream) = self.0.as_ref() else {
            return Ok(true);
        };
        let mut descriptor = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // Poll the WRITE side: on macOS, a read-side HUP also occurs for the
        // normal request half-close, and events=0 does not report peer closure.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(descriptor.revents & (libc::POLLHUP | libc::POLLERR) != 0)
    }

    pub fn finish(&mut self, error: Option<String>) {
        self.finish_with_token(error, None);
    }

    pub fn finish_with_token(&mut self, error: Option<String>, viewer_token: Option<&str>) {
        let Some(mut stream) = self.0.take() else {
            return;
        };
        let error = error.map(|message| {
            if message.len() <= 512 {
                message
            } else {
                format!("{}…", message.chars().take(512).collect::<String>())
            }
        });
        let reply = ReplyBody {
            ok: error.is_none(),
            error: error.as_deref(),
            viewer_token: if error.is_none() { viewer_token } else { None },
        };
        let result = (|| -> io::Result<()> {
            let bytes = serde_json::to_vec(&reply)?;
            stream.set_nonblocking(true)?;
            // A reply fits below 4 KiB. Never wait for an unresponsive client.
            stream.write_all(&bytes)?;
            stream.shutdown(Shutdown::Write)
        })();
        if let Err(error) = result {
            eprintln!("pdfterm: forward reply failed: {error}");
        }
    }
}

impl Drop for ForwardReply {
    fn drop(&mut self) {
        self.finish(Some("viewer stopped before request completion".into()));
    }
}

fn request_reply(
    socket: &str,
    bytes: &[u8],
    kind: &str,
    operation: &crate::process::Operation,
) -> io::Result<()> {
    let mut stream = connect(socket, operation)?;
    send(&mut stream, bytes, operation)?;
    let mut response = [0; 4097];
    let mut length = 0;
    loop {
        operation.check()?;
        match stream.read(&mut response[length..]) {
            Ok(0) => break,
            Ok(n) => {
                length += n;
                if length == response.len() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait_socket(&stream, libc::POLLIN, operation)?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    operation.check()?;
    let response = std::str::from_utf8(&response[..length]).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )
    })?;
    if length > 4096 {
        return Err(io::Error::other(format!("{kind} reply exceeds 4096 bytes")));
    }
    let reply: Reply = serde_json::from_str(response)?;
    operation.check()?;
    if reply.ok {
        Ok(())
    } else {
        Err(io::Error::other(
            reply
                .error
                .unwrap_or_else(|| format!("{kind} request rejected")),
        ))
    }
}

pub fn forward(path: &str, request: &ForwardRequest) -> io::Result<()> {
    let operation = crate::process::Operation::new(FORWARD_TIMEOUT + Duration::from_secs(1));
    request.validate()?;
    request_reply(path, &serde_json::to_vec(request)?, "forward", &operation)
}
fn validate_screenshot_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot output path must be absolute",
        ));
    }
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("screenshot output already exists: {}", path.display()),
        ));
    }
    Ok(())
}

pub fn screenshot(socket: &str, path: &Path) -> io::Result<()> {
    let operation = crate::process::Operation::new(FORWARD_TIMEOUT + Duration::from_secs(1));
    validate_screenshot_path(path)?;
    request_reply(
        socket,
        &serde_json::to_vec(&serde_json::json!({"type": "screenshot", "path": path}))?,
        "screenshot",
        &operation,
    )
}

/// Incremental request reads. A slow client never sleeps on the event thread.
struct Incoming {
    stream: UnixStream,
    bytes: Vec<u8>,
    deadline: Instant,
}
impl Incoming {
    fn poll(&mut self) -> Option<io::Result<String>> {
        let mut buffer = [0; 4097];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    return Some(
                        String::from_utf8(std::mem::take(&mut self.bytes))
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
                    );
                }
                Ok(n) => {
                    if self.bytes.len() + n > 4096 {
                        return Some(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "viewer request exceeds 4096 bytes",
                        )));
                    }
                    self.bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Some(Err(e)),
            }
        }
        (Instant::now() >= self.deadline).then(|| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "viewer request did not reach EOF within 100ms",
            ))
        })
    }
}

pub(crate) struct ForwardListener {
    listener: Listener,
    clients: Vec<Incoming>,
}
impl ForwardListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        Ok(Self {
            listener: Listener::bind(path)?,
            clients: Vec::new(),
        })
    }
    /// Wake modal UI without consuming the request; the main loop owns delivery.
    pub fn has_pending(&self) -> io::Result<bool> {
        use std::os::fd::AsRawFd;
        if !self.clients.is_empty() {
            return Ok(true);
        }
        let mut descriptor = libc::pollfd {
            fd: self.listener.socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut descriptor, 1, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::other("forward listener is unavailable"));
        }
        Ok(descriptor.revents & libc::POLLIN != 0)
    }
    pub fn poll(&mut self) -> io::Result<Vec<(io::Result<ViewerRequest>, ForwardReply)>> {
        for _ in self.clients.len()..16 {
            match self.listener.socket.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true)?;
                    self.clients.push(Incoming {
                        stream,
                        bytes: Vec::new(),
                        deadline: Instant::now() + Duration::from_millis(100),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.clients.len() {
            if let Some(result) = self.clients[index].poll() {
                let client = self.clients.remove(index);
                let request = result.and_then(|payload| {
                    let value: serde_json::Value = serde_json::from_str(&payload)?;
                    match value.get("type").and_then(serde_json::Value::as_str) {
                        Some("screenshot") => {
                            let path = value
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "screenshot request has no path",
                                    )
                                })?;
                            let path = PathBuf::from(path);
                            validate_screenshot_path(&path)?;
                            Ok(ViewerRequest::Screenshot(path))
                        }
                        Some("focus") => parse_focus_request(&payload).map(ViewerRequest::Focus),
                        _ => parse_forward_request(&payload).map(ViewerRequest::Forward),
                    }
                });
                ready.push((request, ForwardReply::new(client.stream)));
            } else {
                index += 1;
            }
        }
        Ok(ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    #[test]
    fn forward_reply_waits_for_submission_and_survives_request_half_close() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        client.set_nonblocking(true).unwrap();
        server.set_nonblocking(true).unwrap();
        let mut reply = ForwardReply::new(server);
        assert!(!reply.disconnected().unwrap());
        let mut byte = [0];
        assert_eq!(
            client.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        reply.finish(None);
        drop(reply);
        client.set_nonblocking(false).unwrap();
        let mut payload = String::new();
        client.read_to_string(&mut payload).unwrap();
        let response: Reply = serde_json::from_str(&payload).unwrap();
        assert!(response.ok);
        assert!(response.error.is_none());
        assert!(response._viewer_token.is_none());
    }

    #[test]
    fn forward_reply_identifies_only_successful_viewer() {
        for error in [None, Some("render failed".to_owned())] {
            let (mut client, server) = UnixStream::pair().unwrap();
            let mut reply = ForwardReply::new(server);
            reply.finish_with_token(error, Some("launched-viewer"));
            let mut payload = String::new();
            client.read_to_string(&mut payload).unwrap();
            let response: Reply = serde_json::from_str(&payload).unwrap();
            assert_eq!(
                response._viewer_token.as_deref(),
                response.ok.then_some("launched-viewer")
            );
        }
    }

    #[test]
    fn focus_request_is_strict_and_replies_without_a_viewer_token() {
        let token = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            parse_focus_request(&format!(r#"{{"type":"focus","viewer_token":"{token}"}}"#))
                .unwrap(),
            token
        );
        for payload in [
            r#"{"type":"focus","viewer_token":"bad"}"#,
            r#"{"type":"focus","viewer_token":"0123456789abcdef0123456789abcdef","extra":true}"#,
            r#"{"type":"screenshot","viewer_token":"0123456789abcdef0123456789abcdef"}"#,
        ] {
            assert_eq!(
                parse_focus_request(payload).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private/focus.sock");
        let mut listener = ForwardListener::bind(&path).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        write!(client, r#"{{"type":"focus","viewer_token":"{token}"}}"#).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let ready = listener.poll().unwrap();
        assert_eq!(ready.len(), 1);
        let (request, mut reply) = ready.into_iter().next().unwrap();
        assert!(matches!(request.unwrap(), ViewerRequest::Focus(value) if value == token));
        reply.finish(None);
        let mut payload = String::new();
        client.read_to_string(&mut payload).unwrap();
        let response: Reply = serde_json::from_str(&payload).unwrap();
        assert!(response.ok);
        assert!(response.error.is_none());
        assert!(response._viewer_token.is_none());
    }

    #[test]
    fn abandoned_forward_reply_reports_failure_and_detects_disconnection() {
        let (mut client, server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        drop(ForwardReply::new(server));
        let mut payload = String::new();
        client.read_to_string(&mut payload).unwrap();
        let response: Reply = serde_json::from_str(&payload).unwrap();
        assert!(!response.ok);
        assert!(response.error.is_some());

        let (client, server) = UnixStream::pair().unwrap();
        let reply = ForwardReply::new(server);
        drop(client);
        assert!(reply.disconnected().unwrap());
    }

    #[test]
    fn escaped_forward_errors_fit_the_reply_limit() {
        let (mut client, server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        ForwardReply::new(server).finish(Some("\0".repeat(2048)));
        let mut payload = String::new();
        client.read_to_string(&mut payload).unwrap();
        assert!(payload.len() <= 4096);
        let response: Reply = serde_json::from_str(&payload).unwrap();
        assert!(!response.ok);
        assert!(response.error.unwrap().starts_with('\0'));
    }

    #[test]
    fn refuses_public_or_symlink_directories() {
        let root = tempfile::tempdir().unwrap();
        let public = root.path().join("public");
        fs::create_dir(&public).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            private_dir(&public).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let link = root.path().join("link");
        symlink(root.path(), &link).unwrap();
        assert_eq!(
            private_dir(&link).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn listener_never_steals_paths_and_cleans_only_itself() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private/viewer.sock");
        let first = Listener::bind(&path).unwrap();
        assert!(Listener::bind(&path).is_err());
        assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        drop(first);
        assert!(!path.exists());
        fs::write(&path, "unrelated").unwrap();
        assert!(Listener::bind(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "unrelated");
        fs::remove_file(&path).unwrap();
        let listener = Listener::bind(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, "replacement").unwrap();
        drop(listener);
        assert_eq!(fs::read_to_string(path).unwrap(), "replacement");
    }

    #[test]
    fn incomplete_clients_do_not_block_complete_requests_or_future_connections() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private/forward.sock");
        let mut listener = ForwardListener::bind(&path).unwrap();
        let mut slow = UnixStream::connect(&path).unwrap();
        slow.write_all(b"{").unwrap();
        assert!(listener.poll().unwrap().is_empty());
        let mut fast = UnixStream::connect(&path).unwrap();
        fast.write_all(b"{}").unwrap();
        fast.shutdown(Shutdown::Write).unwrap();
        let ready = listener.poll().unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(listener.clients.len(), 1);
        assert!(ready[0].0.is_err()); // Complete but invalid JSON request.
        listener.clients[0].deadline = Instant::now();
        let expired = listener.poll().unwrap();
        assert_eq!(
            expired[0].0.as_ref().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        let mut next = UnixStream::connect(&path).unwrap();
        next.write_all(&vec![b'x'; 4097]).unwrap();
        drop(next); // Also exercises a full peer close before acceptance on macOS.
        let oversized = listener.poll().unwrap();
        assert_eq!(oversized.len(), 1);
        assert!(
            oversized[0]
                .0
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("4096")
        );
    }

    #[test]
    fn editor_socket_backpressure_observes_navigation_deadline() {
        let (mut sender, _receiver) = UnixStream::pair().unwrap();
        sender.set_nonblocking(true).unwrap();
        let operation = crate::process::Operation::new(Duration::from_millis(30));
        let error = send(&mut sender, &vec![0; 4 * 1024 * 1024], &operation).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    fn accept_client(listener: &UnixListener) -> UnixStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // macOS can inherit the listener's nonblocking mode.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    return stream;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "client did not connect");
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("client accept failed: {error}"),
            }
        }
    }

    #[test]
    fn manual_clients_exchange_requests_and_wait_for_reply_eof() {
        for kind in ["forward", "screenshot"] {
            let root = tempfile::tempdir().unwrap();
            let socket = root.path().join("client.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let pdf = root.path().join("document.pdf");
            fs::write(&pdf, b"revision fixture").unwrap();
            let request = ForwardRequest {
                revision: crate::synctex::PdfRevision::read(&pdf).unwrap(),
                pdf,
                page: 1,
                h: 10.0,
                v: 20.0,
                width: 5.0,
                height: 5.0,
                word: None,
                inverse_search: None,
            };
            let output = root.path().join("capture.png");
            let expected = if kind == "forward" {
                serde_json::to_value(&request).unwrap()
            } else {
                serde_json::json!({"type": "screenshot", "path": output})
            };
            let (done, result) = std::sync::mpsc::channel();
            let client = std::thread::spawn(move || {
                let response = if kind == "forward" {
                    forward(socket.to_str().unwrap(), &request)
                } else {
                    screenshot(socket.to_str().unwrap(), &output)
                };
                done.send(response).unwrap();
            });
            let mut server = accept_client(&listener);
            let mut bytes = Vec::new();
            server.read_to_end(&mut bytes).unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                expected
            );
            server.write_all(br#"{"ok":true,"error":null}"#).unwrap();
            assert!(matches!(
                result.recv_timeout(Duration::from_millis(20)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            server.shutdown(Shutdown::Write).unwrap();
            result
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            client.join().unwrap();
        }
    }

    #[test]
    fn manual_client_deadline_covers_slow_drip_reply() {
        for kind in ["forward", "screenshot"] {
            let root = tempfile::tempdir().unwrap();
            let socket = root.path().join("drip.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = std::thread::spawn(move || {
                let mut stream = accept_client(&listener);
                stream.read_to_end(&mut Vec::new()).unwrap();
                // Complete JSON alone is not a reply: its EOF must also arrive.
                stream.write_all(br#"{"ok":true,"error":null}"#).unwrap();
                let mut sent = 0;
                for _ in 0..500 {
                    std::thread::sleep(Duration::from_millis(20));
                    match stream.write_all(b" ") {
                        Ok(()) => sent += 1,
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                            ) =>
                        {
                            break;
                        }
                        Err(error) => panic!("slow reply write failed: {error}"),
                    }
                }
                sent
            });
            // Allow loaded CI runners to schedule several writes, while the
            // full drip lasts much longer than the request's total deadline.
            let operation = crate::process::Operation::new(Duration::from_secs(1));
            let started = Instant::now();
            let error =
                request_reply(socket.to_str().unwrap(), b"{}", kind, &operation).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert_eq!(error.to_string(), "navigation timed out");
            assert!(started.elapsed() < Duration::from_secs(3));
            assert!(server.join().unwrap() >= 2);
        }
    }

    #[test]
    fn manual_client_reply_limits_and_errors_are_preserved() {
        for kind in ["forward", "screenshot"] {
            let mut at_limit = br#"{"ok":true}"#.to_vec();
            at_limit.resize(4096, b' ');
            let cases = [
                (at_limit, None),
                (
                    vec![b' '; 4097],
                    Some(format!("{kind} reply exceeds 4096 bytes")),
                ),
                (
                    br#"{"ok":false,"error":"render rejected"}"#.to_vec(),
                    Some("render rejected".into()),
                ),
                (
                    br#"{"ok":false}"#.to_vec(),
                    Some(format!("{kind} request rejected")),
                ),
                (
                    vec![0xff],
                    Some("stream did not contain valid UTF-8".into()),
                ),
            ];
            for (reply, expected) in cases {
                let root = tempfile::tempdir().unwrap();
                let socket = root.path().join("reply.sock");
                let listener = UnixListener::bind(&socket).unwrap();
                let server = std::thread::spawn(move || {
                    let mut stream = accept_client(&listener);
                    stream.read_to_end(&mut Vec::new()).unwrap();
                    stream.write_all(&reply).unwrap();
                });
                let result = request_reply(
                    socket.to_str().unwrap(),
                    b"{}",
                    kind,
                    &crate::process::Operation::new(Duration::from_secs(1)),
                );
                match expected {
                    None => result.unwrap(),
                    Some(expected) => assert_eq!(result.unwrap_err().to_string(), expected),
                }
                server.join().unwrap();
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn saturated_listener() -> (tempfile::TempDir, PathBuf, UnixListener, Vec<UnixStream>) {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("backlog.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
        let queued = if cfg!(target_os = "linux") { 2 } else { 1 };
        let clients = (0..queued)
            .map(|_| {
                connect(
                    socket.to_str().unwrap(),
                    &crate::process::Operation::new(Duration::from_secs(1)),
                )
                .unwrap()
            })
            .collect();
        (root, socket, listener, clients)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn saturated_backlog_obeys_manual_client_deadline() {
        let (_root, socket, _listener, _queued) = saturated_listener();
        for kind in ["forward", "screenshot"] {
            let started = Instant::now();
            let error = request_reply(
                socket.to_str().unwrap(),
                b"{}",
                kind,
                &crate::process::Operation::new(Duration::from_millis(60)),
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert_eq!(error.to_string(), "navigation timed out");
            assert!(started.elapsed() < Duration::from_millis(500));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn saturated_backlog_connect_retries_until_accepted() {
        let (_root, socket, listener, _queued) = saturated_listener();
        let client = std::thread::spawn(move || {
            request_reply(
                socket.to_str().unwrap(),
                b"{}",
                "screenshot",
                &crate::process::Operation::new(Duration::from_secs(1)),
            )
        });
        std::thread::sleep(Duration::from_millis(30));
        // Linux returned EAGAIN without queueing this client's connection.
        drop(accept_client(&listener));
        drop(accept_client(&listener));
        let mut server = accept_client(&listener);
        let mut bytes = Vec::new();
        server.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"{}");
        server.write_all(br#"{"ok":true}"#).unwrap();
        server.shutdown(Shutdown::Write).unwrap();
        client.join().unwrap().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn saturated_macos_backlog_keeps_immediate_refusal() {
        let (_root, socket, _listener, _queued) = saturated_listener();
        for kind in ["forward", "screenshot"] {
            let started = Instant::now();
            let error = request_reply(
                socket.to_str().unwrap(),
                b"{}",
                kind,
                &crate::process::Operation::new(Duration::from_millis(60)),
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
            assert!(started.elapsed() < Duration::from_millis(500));
        }
    }
}
