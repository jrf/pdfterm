use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

pub(crate) const FOCUS_TIMEOUT: Duration = Duration::from_secs(6);
const HELPER_TIMEOUT: Duration = Duration::from_secs(3);
const REPLY_LIMIT: usize = 4096;

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
    let address = parse_endpoint(&endpoint)?;
    let token = read_token(&token_path)?;
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
    operation.check()?;
    let mut response = Vec::with_capacity(256);
    let mut chunk = [0_u8; 512];
    loop {
        let timeout = remaining()?.min(Duration::from_millis(50));
        let mut descriptor = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout.as_millis().max(1) as i32) };
        if ready == 0 {
            continue;
        }
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(bridge_error("poll", error));
        }
        let count = stream
            .read(&mut chunk)
            .map_err(|e| bridge_error("read", e))?;
        if count == 0 {
            break;
        }
        if response.len() + count > REPLY_LIMIT {
            return Err(invalid_data("SSH focus bridge reply exceeds 4096 bytes"));
        }
        response.extend_from_slice(&chunk[..count]);
    }
    operation.check()?;
    let reply: BridgeReply = serde_json::from_slice(&response).map_err(invalid_data)?;
    if reply.ok {
        Ok(())
    } else {
        Err(io::Error::other(reply.error.unwrap_or_else(|| {
            "SSH focus bridge rejected request".into()
        })))
    }
}

fn parse_endpoint(endpoint: &str) -> io::Result<SocketAddr> {
    let authority = endpoint
        .strip_prefix("tcp://")
        .ok_or_else(|| invalid("SSH focus endpoint must use tcp://127.0.0.1:<port>"))?;
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
    Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
}

fn read_token(path: &str) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(invalid("SSH focus token path is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid("SSH focus token file must be private (mode 0600)"));
        }
    }
    let mut bytes = Vec::with_capacity(66);
    fs::File::open(path)?.take(67).read_to_end(&mut bytes)?;
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

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{parse_endpoint, valid_uuid};

    #[test]
    fn bridge_endpoint_is_loopback_only() {
        assert_eq!(parse_endpoint("tcp://127.0.0.1:1234").unwrap().port(), 1234);
        for endpoint in [
            "tcp://localhost:1234",
            "tcp://127.0.0.2:1234",
            "tcp://[::1]:1234",
            "tcp://127.0.0.1:0",
            "tcp://127.0.0.1:65536",
        ] {
            assert!(parse_endpoint(endpoint).is_err(), "{endpoint}");
        }
    }

    #[test]
    fn ghostty_handle_requires_uuid_shape() {
        assert!(valid_uuid("01234567-89ab-cdef-0123-456789abcdef"));
        assert!(!valid_uuid("123456"));
        assert!(!valid_uuid("01234567-89ab-cdef-0123-456789abcdeg"));
    }
}
