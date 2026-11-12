//! Resolve compiler-mapped PDF points without changing editor delivery.
use crate::{
    process::Operation,
    synctex::{InversePoint, InverseResolution, PdfRevision, SourceLocation},
};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

pub(crate) fn resolve_inverse(
    endpoint: &str,
    revision: PdfRevision,
    point: InversePoint,
    operation: &Operation,
) -> io::Result<InverseResolution> {
    operation.check()?;
    // The adapter owns a private, local resolver. Do not contact a replaced
    // symlink, another user's socket, or a publicly accessible endpoint.
    crate::ipc::check_private_socket(Path::new(endpoint))?;
    #[derive(Serialize)]
    struct Request {
        revision: PdfRevision,
        page: u32,
        x: f32,
        y: f32,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reply {
        ok: bool,
        location: Option<SourceLocation>,
        error: Option<String>,
    }
    let request = serde_json::to_vec(&Request {
        revision,
        page: point.page,
        x: point.x,
        y: point.y_from_top,
    })?;
    let reply: Reply = crate::ipc::exchange(endpoint, &request, "Typst source-map", operation)?;
    if !reply.ok {
        return Err(io::Error::other(
            reply
                .error
                .unwrap_or_else(|| "Typst source position unavailable".into()),
        ));
    }
    let location = reply.location.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Typst source-map reply has no location",
        )
    })?;
    if !Path::new(&location.file).is_absolute()
        || location.file.as_bytes().contains(&0)
        || location.line == 0
        || location.column == 0
        || location.column_char == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Typst source location",
        ));
    }
    Ok(InverseResolution {
        location,
        warning: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};
    use std::time::Duration;

    fn point() -> InversePoint {
        InversePoint {
            page: 1,
            x: 12.0,
            y_from_top: 20.0,
            page_height_pt: 600.0,
        }
    }

    #[test]
    fn inverse_rejects_public_or_symlinked_endpoints() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("map.sock");
        let _listener = UnixListener::bind(&endpoint).unwrap();
        std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
        let revision = PdfRevision::read(directory.path()).unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = resolve_inverse(
            endpoint.to_str().unwrap(),
            revision,
            point(),
            &Operation::default(),
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = directory.path().join("alias.sock");
        std::os::unix::fs::symlink(&endpoint, &alias).unwrap();
        let error = resolve_inverse(
            alias.to_str().unwrap(),
            revision,
            point(),
            &Operation::default(),
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn inverse_deadline_bounds_an_unresponsive_compiler() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = directory.path().join("map.sock");
        let _listener = UnixListener::bind(&endpoint).unwrap();
        std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
        let revision = PdfRevision::read(directory.path()).unwrap();
        let error = resolve_inverse(
            endpoint.to_str().unwrap(),
            revision,
            point(),
            &Operation::new(Duration::from_millis(50)),
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
