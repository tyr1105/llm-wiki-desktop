//! Frozen directory identities; replacing a startup root never expands authority.
use crate::errors::BackendError;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub(crate) struct FrozenRoot {
    pub path: PathBuf,
    handle: Arc<same_file::Handle>,
}
impl FrozenRoot {
    pub fn capture(path: &Path) -> Result<Self, BackendError> {
        let canonical = path.canonicalize().map_err(scope_error)?;
        if !canonical.is_dir() {
            return Err(scope_denied());
        }
        let handle = same_file::Handle::from_path(&canonical).map_err(scope_error)?;
        Ok(Self {
            path: canonical,
            handle: Arc::new(handle),
        })
    }
    pub fn validate(&self) -> Result<(), BackendError> {
        if self.path.canonicalize().map_err(scope_error)? != self.path {
            return Err(scope_denied());
        }
        let current = same_file::Handle::from_path(&self.path).map_err(scope_error)?;
        if *self.handle != current {
            return Err(scope_denied());
        }
        Ok(())
    }
    pub fn contains(&self, path: &Path) -> Result<bool, BackendError> {
        self.validate()?;
        Ok(path.starts_with(&self.path))
    }
}
fn scope_error(_: std::io::Error) -> BackendError {
    scope_denied()
}
fn scope_denied() -> BackendError {
    BackendError::new("API_STARTUP_ROOT_CHANGED","Startup directory identity changed or is unavailable. Restart with an explicitly reviewed scope.",true,true)
}
pub(crate) fn validate_import_path(
    path: &Path,
    roots: &[FrozenRoot],
) -> Result<PathBuf, BackendError> {
    if !path.is_absolute() {
        return Err(BackendError::new(
            "API_IMPORT_SCOPE_DENIED",
            "Use an absolute import path.",
            true,
            true,
        ));
    }
    let canonical = path.canonicalize().map_err(scope_error)?;
    for root in roots {
        if root.contains(&canonical)? {
            return Ok(canonical);
        }
    }
    Err(BackendError::new(
        "API_IMPORT_SCOPE_DENIED",
        "Import path is outside the frozen startup allowlist.",
        true,
        true,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{import_v2_file::FileScanPolicy, paths::ProjectContext},
        services::import_v2::file_discovery::FileDiscoveryService,
    };
    #[cfg(unix)]
    #[test]
    fn asynchronous_discovery_rechecks_frozen_scope_before_fingerprinting() {
        let parent = tempfile::tempdir().unwrap();
        let incoming = parent.path().join("incoming");
        let private = parent.path().join("private");
        std::fs::create_dir(&incoming).unwrap();
        std::fs::create_dir(&private).unwrap();
        std::fs::write(incoming.join("source.md"), "approved").unwrap();
        std::fs::write(private.join("source.md"), "private").unwrap();
        let scope = vec![FrozenRoot::capture(&incoming).unwrap()];
        let requested = validate_import_path(&incoming.join("source.md"), &scope).unwrap();
        std::fs::rename(&incoming, parent.path().join("previous")).unwrap();
        std::os::unix::fs::symlink(&private, &incoming).unwrap();
        let kb = tempfile::tempdir().unwrap();
        let context = ProjectContext::new("scope-test", kb.path().to_path_buf());
        let mut published = 0;
        let result = FileDiscoveryService.scan_with_guard(
            &context,
            &[requested],
            FileScanPolicy::default(),
            |batch| published += batch.len(),
            || false,
            |path| validate_import_path(path, &scope).map(|_| ()),
        );
        assert!(result.is_err());
        assert_eq!(published, 0);
    }
    #[test]
    fn replacing_directory_without_symlink_also_revokes_frozen_scope() {
        let parent = tempfile::tempdir().unwrap();
        let incoming = parent.path().join("incoming");
        std::fs::create_dir(&incoming).unwrap();
        let scope = FrozenRoot::capture(&incoming).unwrap();
        std::fs::rename(&incoming, parent.path().join("old")).unwrap();
        std::fs::create_dir(&incoming).unwrap();
        assert!(scope.validate().is_err());
    }
}

pub(crate) fn input_guard(
    roots: Vec<FrozenRoot>,
) -> crate::services::import_v2::file_discovery::ImportInputGuard {
    Arc::new(move |input| {
        use crate::models::import_v2::ImportInputKind;
        match input.kind {
            ImportInputKind::File => {
                validate_import_path(Path::new(&input.locator), &roots)?;
                if let Some(path) = &input.normalized_locator {
                    validate_import_path(Path::new(path), &roots)?;
                }
                if let Some(identity) = &input.source_identity {
                    validate_import_path(Path::new(&identity.canonical_path), &roots)?;
                }
            }
            ImportInputKind::Folder => {
                return Err(BackendError::new(
                    "API_FOLDER_PARSE_UNSUPPORTED",
                    "Use /paths to discover individual files with a scope-bound scan.",
                    true,
                    true,
                ))
            }
            ImportInputKind::Url | ImportInputKind::ClipboardText => {}
        }
        Ok(())
    })
}

#[cfg(test)]
mod input_tests {
    use super::*;
    use crate::models::import_v2::{ImportInput, ImportInputKind, MediaSaveMode};
    fn input(kind: ImportInputKind, locator: String) -> ImportInput {
        ImportInput {
            kind,
            display_name: "test".into(),
            locator,
            normalized_locator: None,
            source_identity: None,
            media_save_mode: MediaSaveMode::ExtractOnly,
        }
    }
    #[test]
    fn legacy_outside_file_is_denied_while_text_and_url_remain_available() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("source.md"), "outside").unwrap();
        let guard = input_guard(vec![FrozenRoot::capture(root.path()).unwrap()]);
        assert!(guard(&input(
            ImportInputKind::File,
            outside
                .path()
                .join("source.md")
                .to_string_lossy()
                .into_owned()
        ))
        .is_err());
        assert!(guard(&input(ImportInputKind::ClipboardText, "clipboard".into())).is_ok());
        assert!(guard(&input(
            ImportInputKind::Url,
            "https://example.com/source".into()
        ))
        .is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn parsing_worker_denies_directory_redirect_after_http_preflight() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("incoming");
        let private = parent.path().join("private");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&private).unwrap();
        std::fs::write(root.join("source.md"), "approved").unwrap();
        std::fs::write(private.join("source.md"), "private").unwrap();
        let guard = input_guard(vec![FrozenRoot::capture(&root).unwrap()]);
        let selected = input(
            ImportInputKind::File,
            root.join("source.md").to_string_lossy().into_owned(),
        );
        guard(&selected).unwrap();
        std::fs::rename(&root, parent.path().join("old")).unwrap();
        std::os::unix::fs::symlink(&private, &root).unwrap();
        assert!(guard(&selected).is_err());
    }
}
