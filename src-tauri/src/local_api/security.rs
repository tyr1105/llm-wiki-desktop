use super::ApiError;
use axum::http::{HeaderMap, StatusCode};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

pub(super) fn authorize(headers: &HeaderMap, token: &str, port: u16) -> Result<(), ApiError> {
    if headers.contains_key("origin") {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "API_BROWSER_ORIGIN_DENIED",
            "Browser origins are not allowed.",
        ));
    }
    let expected = format!("127.0.0.1:{port}");
    if headers.get("host").and_then(|h| h.to_str().ok()) != Some(expected.as_str()) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "API_HOST_DENIED",
            "Unexpected listener authority.",
        ));
    }
    let supplied = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    let a = Sha256::digest(supplied.as_bytes());
    let b = Sha256::digest(token.as_bytes());
    let difference = a
        .iter()
        .zip(b.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    if difference != 0 {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "API_AUTH_REQUIRED",
            "A valid Bearer token is required.",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct InstanceLease {
    _file: File,
}
impl InstanceLease {
    pub fn acquire(config_dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(config_dir)?;
        let path = config_dir.join("backend-instance.lock");
        if path
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Instance lock cannot be a symlink.",
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        file.try_lock_exclusive()?;
        Ok(Self { _file: file })
    }
}

pub(super) fn allowed_import_path(
    path: &Path,
    roots: &[super::scope::FrozenRoot],
) -> Result<PathBuf, ApiError> {
    super::scope::validate_import_path(path, roots).map_err(ApiError::from)
}
