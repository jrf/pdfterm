use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

pub(crate) const FOCUS_TIMEOUT: Duration = Duration::from_secs(6);
const HELPER_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Deserialize)]
struct BridgeReply {
    ok: bool,
    error: Option<String>,
}

pub(crate) fn focus_self(operation: &crate::process::Operation) -> io::Result<()> {
    operation.check()?;
    if in_ssh()
        || env::var_os("PDFTERM_LAUNCH_SOCKET").is_some()
        || env::var_os("PDFTERM_LAUNCH_TOKEN_FILE").is_some()
    {
        return focus_ssh(operation);
    }
    if let Some(id) = env::var_os("PDFTERM_VIEWER_TERMINAL_ID") {
        let id = id.to_string_lossy();
        if !valid_uuid(&id) {
            return Err(invalid("PDFTERM_VIEWER_TERMINAL_ID must be a UUID"));
        }
        #[cfg(target_os = "macos")]
        return helper(
            operation,
            "osascript",
            &[
                "-e",
                "on run argv\ntell application \"Ghostty\"\nfocus terminal id (item 1 of argv)\nend tell\nend run",
                &id,
            ],
        );
        #[cfg(not(target_os = "macos"))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Ghostty terminal focus is supported only on macOS",
        ));
    }
    if let Some(id) = env::var_os("KITTY_WINDOW_ID") {
        let id = id.to_string_lossy();
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid("KITTY_WINDOW_ID must be a numeric window ID"));
        }
        return helper(
            operation,
            "kitten",
            &["@", "focus-window", "--match", &format!("id:{id}")],
        );
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no exact terminal handle; launch pdfterm through scripts/pdfterm-viewer",
    ))
}

fn in_ssh() -> bool {
    env::var_os("SSH_CONNECTION").is_some()
        || env::var_os("SSH_CLIENT").is_some()
        || env::var_os("SSH_TTY").is_some()
}

#[derive(Debug, PartialEq, Eq)]
enum Endpoint<'a> {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(&'a str),
}

fn focus_ssh(operation: &crate::process::Operation) -> io::Result<()> {
    let endpoint = env::var("PDFTERM_LAUNCH_SOCKET").map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "SSH viewer focus requires PDFTERM_LAUNCH_SOCKET and PDFTERM_LAUNCH_TOKEN_FILE",
        )
    })?;
    let token_path = env::var("PDFTERM_LAUNCH_TOKEN_FILE").map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "SSH viewer focus requires PDFTERM_LAUNCH_SOCKET and PDFTERM_LAUNCH_TOKEN_FILE",
        )
    })?;
    focus_ssh_with(operation, &endpoint, &token_path)
}

fn focus_ssh_with(
    operation: &crate::process::Operation,
    endpoint: &str,
    token_path: &str,
) -> io::Result<()> {
    let endpoint = parse_endpoint(endpoint)?;
    let token = read_token(token_path)?;
    let request = serde_json::json!({"action":"focus", "id":"source", "token":token});
    let mut bytes = serde_json::to_vec(&request).map_err(invalid_data)?;
    bytes.push(b'\n');

    let remaining = || {
        operation.remaining().map_err(|error| {
            if error.kind() == io::ErrorKind::TimedOut {
                io::Error::new(io::ErrorKind::TimedOut, "SSH focus bridge timed out")
            } else {
                error
            }
        })
    };
    let reply: BridgeReply = match endpoint {
        #[cfg(unix)]
        Endpoint::Unix(path) => crate::ipc::exchange(path, &bytes, "SSH focus bridge", operation)
            .map_err(|e| bridge_operation_error("request", e))?,
        Endpoint::Tcp(address) => {
            let mut stream =
                TcpStream::connect_timeout(&address, remaining()?.max(Duration::from_millis(1)))
                    .map_err(|e| bridge_error("connect", e))?;
            stream
                .set_write_timeout(Some(remaining()?.max(Duration::from_millis(1))))
                .map_err(|e| bridge_error("write timeout", e))?;
            stream
                .write_all(&bytes)
                .map_err(|e| bridge_error("write", e))?;
            stream
                .shutdown(std::net::Shutdown::Write)
                .map_err(|e| bridge_error("shutdown", e))?;
            stream
                .set_nonblocking(true)
                .map_err(|e| bridge_error("nonblocking", e))?;
            crate::ipc::read_json_reply(&mut stream, "SSH focus bridge", operation)
                .map_err(|e| bridge_operation_error("read", e))?
        }
    };
    if reply.ok {
        Ok(())
    } else {
        Err(io::Error::other(reply.error.unwrap_or_else(|| {
            "SSH focus bridge rejected request".into()
        })))
    }
}

fn parse_endpoint(endpoint: &str) -> io::Result<Endpoint<'_>> {
    #[cfg(unix)]
    if endpoint.starts_with('/') {
        if !crate::ipc::valid_socket_path(endpoint) {
            return Err(invalid("SSH focus Unix socket path is invalid"));
        }
        return Ok(Endpoint::Unix(endpoint));
    }
    let authority = endpoint.strip_prefix("tcp://").ok_or_else(|| {
        invalid(
            "SSH focus endpoint must use tcp://127.0.0.1:<port> or an absolute Unix socket path",
        )
    })?;
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| invalid("SSH focus endpoint must use tcp://127.0.0.1:<port>"))?;
    if host != "127.0.0.1" || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid(
            "SSH focus endpoint must use tcp://127.0.0.1:<port>",
        ));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| invalid("SSH focus endpoint port is invalid"))?;
    if port == 0 {
        return Err(invalid("SSH focus endpoint port is invalid"));
    }
    Ok(Endpoint::Tcp(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        port,
    )))
}

fn read_token(path: &str) -> io::Result<String> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(invalid("SSH focus token path is not a regular file"));
        }
        if !crate::ipc::is_owned_with_mask(&metadata, 0o077) {
            return Err(invalid("SSH focus token file must be private (mode 0600)"));
        }
        file
    };
    #[cfg(not(unix))]
    let file = {
        let file = fs::File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(invalid("SSH focus token path is not a regular file"));
        }
        file
    };
    let mut bytes = Vec::with_capacity(66);
    file.take(67).read_to_end(&mut bytes)?;
    if bytes.len() > 66 {
        return Err(invalid(
            "SSH focus token file exceeds 64 hexadecimal characters",
        ));
    }
    let token = std::str::from_utf8(&bytes).map_err(invalid_data)?;
    let token = token
        .strip_suffix("\r\n")
        .or_else(|| token.strip_suffix('\n'))
        .unwrap_or(token);
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(
            "SSH focus token file must contain a 64-character hexadecimal token",
        ));
    }
    Ok(token.to_owned())
}

fn helper(operation: &crate::process::Operation, program: &str, args: &[&str]) -> io::Result<()> {
    let output = crate::process::output(
        Command::new(program).args(args),
        &operation.limited(HELPER_TIMEOUT),
    )?;
    operation.check()?;
    if output.status.success() {
        Ok(())
    } else {
        let message = if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        };
        Err(io::Error::other(format!(
            "{program} focus failed: {}",
            String::from_utf8_lossy(message).trim()
        )))
    }
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn bridge_error(stage: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("SSH focus bridge {stage}: {error}"))
}

fn bridge_operation_error(stage: &str, error: io::Error) -> io::Error {
    match error.kind() {
        io::ErrorKind::TimedOut => {
            io::Error::new(io::ErrorKind::TimedOut, "SSH focus bridge timed out")
        }
        io::ErrorKind::Interrupted => error,
        _ => bridge_error(stage, error),
    }
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Endpoint, FOCUS_TIMEOUT, focus_ssh_with, parse_endpoint, read_token, valid_uuid};

    #[test]
    fn bridge_endpoint_accepts_loopback_tcp_and_unix_paths() {
        match parse_endpoint("tcp://127.0.0.1:1234").unwrap() {
            Endpoint::Tcp(addr) => assert_eq!(addr.port(), 1234),
            #[cfg(unix)]
            Endpoint::Unix(_) => panic!("expected TCP endpoint"),
        }
        #[cfg(unix)]
        assert_eq!(
            parse_endpoint("/tmp/pdfterm-abc123/launch.sock").unwrap(),
            Endpoint::Unix("/tmp/pdfterm-abc123/launch.sock")
        );
        for endpoint in [
            "tcp://localhost:1234",
            "tcp://127.0.0.2:1234",
            "tcp://[::1]:1234",
            "tcp://127.0.0.1:0",
            "tcp://127.0.0.1:65536",
            "relative/launch.sock",
            "",
        ] {
            assert!(parse_endpoint(endpoint).is_err(), "{endpoint}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_ssh_focus_validates_token_and_completes_roundtrip() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;
        use std::path::PathBuf;
        use std::time::Duration;

        let dir = PathBuf::from(format!(
            "/tmp/pdfterm-focus-unix-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("launch.sock");
        let token_path = dir.join("launch.token");
        let hex = "fedcba9876543210".repeat(4);
        std::fs::write(&token_path, format!("{hex}\n")).unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let listener = UnixListener::bind(&sock_path).unwrap();
        let expected_token = hex.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream).read_line(&mut line).unwrap();
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["action"], "focus");
            assert_eq!(req["id"], "source");
            assert_eq!(req["token"], expected_token);
            stream.write_all(b"{\"ok\":true}\n").unwrap();
        });

        let op = crate::process::Operation::new(FOCUS_TIMEOUT);
        focus_ssh_with(
            &op,
            sock_path.to_str().unwrap(),
            token_path.to_str().unwrap(),
        )
        .unwrap();
        server.join().unwrap();

        let expired_op = crate::process::Operation::new(Duration::ZERO);
        let err = focus_ssh_with(
            &expired_op,
            sock_path.to_str().unwrap(),
            token_path.to_str().unwrap(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ghostty_handle_requires_uuid_shape() {
        assert!(valid_uuid("01234567-89ab-cdef-0123-456789abcdef"));
        assert!(!valid_uuid("123456"));
        assert!(!valid_uuid("01234567-89ab-cdef-0123-456789abcdeg"));
    }

    #[cfg(unix)]
    #[test]
    fn token_file_requires_private_regular_file_without_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = std::env::temp_dir().join(format!(
            "pdfterm-focus-token-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let token_path = dir.join("token");
        let link_path = dir.join("token-link");
        let hex = "0123456789abcdef".repeat(4);
        std::fs::write(&token_path, format!("{hex}\n")).unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token(token_path.to_str().unwrap()).is_err());

        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token(token_path.to_str().unwrap()).unwrap(), hex);

        symlink(&token_path, &link_path).unwrap();
        assert!(read_token(link_path.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
