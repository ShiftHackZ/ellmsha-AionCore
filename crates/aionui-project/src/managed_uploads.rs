use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use aionui_db::IConversationRepository;

use crate::types::ProjectError;

const GENERAL_SCOPE: &str = "general";
const MAX_COLLISIONS: u32 = 1000;

/// Durable, profile-owned storage for browser uploads.
///
/// Encoded user and conversation directories make the current user's root an
/// enforceable authorization boundary without placing untrusted identifiers in
/// filesystem components.
#[derive(Clone)]
pub struct ManagedUploadStore {
    root: PathBuf,
    legacy_root: PathBuf,
    conversation_repo: Option<Arc<dyn IConversationRepository>>,
}

impl ManagedUploadStore {
    pub fn new(root: PathBuf, conversation_repo: Arc<dyn IConversationRepository>) -> Self {
        Self {
            root,
            legacy_root: std::env::temp_dir().join("aionui"),
            conversation_repo: Some(conversation_repo),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: PathBuf, legacy_root: PathBuf) -> Self {
        Self {
            root,
            legacy_root,
            conversation_repo: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn is_legacy_path(&self, path: &str) -> bool {
        legacy_parts(&self.legacy_root, Path::new(path)).is_some()
    }

    pub async fn create(
        &self,
        user_id: &str,
        file_name: &str,
        data: &[u8],
        conversation_id: Option<&str>,
    ) -> Result<String, ProjectError> {
        validate_leaf(file_name, "file name")?;
        if user_id.is_empty() {
            return Err(ProjectError::InvalidUploadIdentity);
        }
        let scope = conversation_id
            .filter(|value| !value.is_empty())
            .unwrap_or(GENERAL_SCOPE);
        let storage_root = self.root.clone();
        let directory = self.scope_root(user_id, scope);
        let file_name = file_name.to_owned();
        let data = data.to_vec();

        tokio::task::spawn_blocking(move || {
            ensure_private_directory(&storage_root, &directory)?;
            create_collision_safe_file(&directory, &file_name, &data)
        })
        .await
        .map_err(|error| ProjectError::UploadStorageIo {
            message: format!("upload task failed: {error}"),
        })?
        .and_then(|path| std::fs::canonicalize(path).map_err(upload_io))
        .map(|path| path.to_string_lossy().into_owned())
    }

    /// Resolve a managed upload for `user_id`. A still-existing legacy temp
    /// upload is copied into durable storage only after its conversation
    /// directory is proven to belong to this user.
    pub async fn resolve(&self, user_id: &str, path: &str) -> Result<String, ProjectError> {
        let candidate = Path::new(path);
        if !candidate.is_absolute() {
            return Err(ProjectError::UploadPathOutsideRoot { path: path.to_owned() });
        }

        let user_root = self.resolved_user_root(user_id);
        if lexical_descendant(&user_root, candidate) {
            return resolve_existing_managed(&user_root, candidate, path);
        }

        let Some((scope, file_name)) = legacy_parts(&self.legacy_root, candidate) else {
            return Err(ProjectError::UploadPathOutsideRoot { path: path.to_owned() });
        };
        // The old shared `general` bucket carries no user or profile identity.
        // It is adoptable only while processing a reference already persisted
        // in this profile's user-scoped message history (see `adopt_persisted`).
        if scope == GENERAL_SCOPE {
            return Err(ProjectError::LegacyUploadOwnerUnknown);
        }
        self.authorize_legacy_scope(user_id, &scope).await?;
        self.adopt_authorized(user_id, &scope, &file_name, candidate, path)
            .await
    }

    /// Adopt an exact legacy upload path referenced by a persisted message.
    /// The caller supplies the owning message's conversation ID, making a
    /// legacy `general` reference attributable to this profile and user while
    /// refusing references to another conversation's legacy directory.
    pub(crate) async fn adopt_persisted(
        &self,
        user_id: &str,
        conversation_id: &str,
        path: &str,
    ) -> Result<String, ProjectError> {
        let candidate = Path::new(path);
        let Some((scope, file_name)) = legacy_parts(&self.legacy_root, candidate) else {
            return Err(ProjectError::UploadPathOutsideRoot { path: path.to_owned() });
        };
        if scope != GENERAL_SCOPE && scope != conversation_id {
            return Err(ProjectError::LegacyUploadOwnerUnknown);
        }
        if scope != GENERAL_SCOPE {
            self.authorize_legacy_scope(user_id, &scope).await?;
        }
        self.adopt_authorized(user_id, &scope, &file_name, candidate, path)
            .await
    }

    async fn adopt_authorized(
        &self,
        user_id: &str,
        scope: &str,
        file_name: &str,
        candidate: &Path,
        original: &str,
    ) -> Result<String, ProjectError> {
        if !candidate.exists() {
            return Err(ProjectError::ChatFileMissing {
                path: original.to_owned(),
            });
        }
        reject_symlink_path(&self.legacy_root, candidate, original)?;
        let canonical_root = std::fs::canonicalize(&self.legacy_root).map_err(|_| ProjectError::ChatFileMissing {
            path: original.to_owned(),
        })?;
        let canonical_source = std::fs::canonicalize(candidate).map_err(|_| ProjectError::ChatFileMissing {
            path: original.to_owned(),
        })?;
        if !canonical_source.starts_with(&canonical_root) || !canonical_source.is_file() {
            return Err(ProjectError::UploadPathOutsideRoot {
                path: original.to_owned(),
            });
        }

        let storage_root = self.root.clone();
        let destination = self.scope_root(user_id, scope);
        let file_name = file_name.to_owned();
        tokio::task::spawn_blocking(move || {
            ensure_private_directory(&storage_root, &destination)?;
            adopt_collision_safe_file(&canonical_source, &destination, &file_name)
        })
        .await
        .map_err(|error| ProjectError::UploadStorageIo {
            message: format!("legacy upload adoption task failed: {error}"),
        })?
        .and_then(|path| std::fs::canonicalize(path).map_err(upload_io))
        .map(|path| path.to_string_lossy().into_owned())
    }

    async fn authorize_legacy_scope(&self, user_id: &str, scope: &str) -> Result<(), ProjectError> {
        let Some(repo) = &self.conversation_repo else {
            return Err(ProjectError::LegacyUploadOwnerUnknown);
        };
        if repo.get(user_id, scope).await?.is_some() {
            Ok(())
        } else {
            Err(ProjectError::LegacyUploadOwnerUnknown)
        }
    }

    fn user_root(&self, user_id: &str) -> PathBuf {
        self.root.join("users").join(encode_component(user_id))
    }

    fn resolved_user_root(&self, user_id: &str) -> PathBuf {
        std::fs::canonicalize(&self.root)
            .unwrap_or_else(|_| self.root.clone())
            .join("users")
            .join(encode_component(user_id))
    }

    fn scope_root(&self, user_id: &str, scope: &str) -> PathBuf {
        self.user_root(user_id)
            .join("conversations")
            .join(encode_component(scope))
    }
}

fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn validate_leaf(value: &str, field: &str) -> Result<(), ProjectError> {
    let mut components = Path::new(value).components();
    let valid = matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if value.is_empty() || !valid || value.contains(['/', '\0', '\\']) {
        return Err(ProjectError::InvalidUploadName {
            field: field.to_owned(),
        });
    }
    Ok(())
}

fn lexical_descendant(root: &Path, target: &Path) -> bool {
    target.strip_prefix(root).is_ok_and(|relative| {
        relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
            && relative.components().next().is_some()
    })
}

fn legacy_parts(root: &Path, target: &Path) -> Option<(String, String)> {
    let relative = target.strip_prefix(root).ok()?;
    let mut components = relative.components();
    let scope = match components.next()? {
        Component::Normal(value) => value.to_str()?.to_owned(),
        _ => return None,
    };
    let file_name = match components.next()? {
        Component::Normal(value) => value.to_str()?.to_owned(),
        _ => return None,
    };
    if components.next().is_some() || scope.is_empty() || file_name.is_empty() {
        return None;
    }
    Some((scope, file_name))
}

fn resolve_existing_managed(user_root: &Path, candidate: &Path, original: &str) -> Result<String, ProjectError> {
    if !candidate.exists() {
        return Err(ProjectError::ChatFileMissing {
            path: original.to_owned(),
        });
    }
    reject_symlink_path(user_root, candidate, original)?;
    let canonical_root = std::fs::canonicalize(user_root).map_err(|_| ProjectError::ChatFileMissing {
        path: original.to_owned(),
    })?;
    let canonical = std::fs::canonicalize(candidate).map_err(|_| ProjectError::ChatFileMissing {
        path: original.to_owned(),
    })?;
    if !canonical.starts_with(canonical_root) || !canonical.is_file() {
        return Err(ProjectError::UploadPathOutsideRoot {
            path: original.to_owned(),
        });
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn reject_symlink_path(root: &Path, target: &Path, original: &str) -> Result<(), ProjectError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| ProjectError::UploadPathOutsideRoot {
            path: original.to_owned(),
        })?;
    let mut current = root.to_path_buf();
    if std::fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ProjectError::UploadPathOutsideRoot {
            path: original.to_owned(),
        });
    }
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(ProjectError::UploadPathOutsideRoot {
                path: original.to_owned(),
            });
        };
        current.push(component);
        if std::fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(ProjectError::UploadPathOutsideRoot {
                path: original.to_owned(),
            });
        }
    }
    Ok(())
}

fn ensure_private_directory(storage_root: &Path, directory: &Path) -> Result<(), ProjectError> {
    let relative = directory
        .strip_prefix(storage_root)
        .map_err(|_| ProjectError::UploadStorageIo {
            message: "managed upload directory escaped its storage root".to_owned(),
        })?;
    let mut current = storage_root.to_path_buf();
    ensure_real_private_directory(&current)?;
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(ProjectError::UploadStorageIo {
                message: "managed upload directory contains an invalid component".to_owned(),
            });
        };
        current.push(value);
        ensure_real_private_directory(&current)?;
    }
    Ok(())
}

fn ensure_real_private_directory(path: &Path) -> Result<(), ProjectError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ProjectError::UploadStorageIo {
                message: "managed upload path contains a non-directory or symbolic link".to_owned(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match std::fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(path).map_err(upload_io)?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(ProjectError::UploadStorageIo {
                        message: "managed upload path raced with a non-directory or symbolic link".to_owned(),
                    });
                }
            }
            Err(error) => return Err(upload_io(error)),
        },
        Err(error) => return Err(upload_io(error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(upload_io)?;
    }
    Ok(())
}

fn create_collision_safe_file(directory: &Path, file_name: &str, data: &[u8]) -> Result<PathBuf, ProjectError> {
    let (base, extension) = split_base_ext(file_name);
    for counter in 1..=MAX_COLLISIONS {
        let candidate_name = if counter == 1 {
            file_name.to_owned()
        } else {
            format!("{base}({counter}){extension}")
        };
        let path = directory.join(candidate_name);
        match open_private_new(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(data).and_then(|_| file.sync_all()) {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(upload_io(error));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(upload_io(error)),
        }
    }
    Err(ProjectError::UploadStorageIo {
        message: format!("too many upload name collisions for '{file_name}'"),
    })
}

fn adopt_collision_safe_file(source: &Path, directory: &Path, file_name: &str) -> Result<PathBuf, ProjectError> {
    let (base, extension) = split_base_ext(file_name);
    for counter in 1..=MAX_COLLISIONS {
        let candidate_name = if counter == 1 {
            file_name.to_owned()
        } else {
            format!("{base}({counter}){extension}")
        };
        let path = directory.join(candidate_name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_symlink() && metadata.is_file() && files_equal(source, &path)? {
                    return Ok(path);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut input = File::open(source).map_err(upload_io)?;
                match open_private_new(&path) {
                    Ok(mut output) => {
                        if let Err(error) = std::io::copy(&mut input, &mut output).and_then(|_| output.sync_all()) {
                            drop(output);
                            let _ = std::fs::remove_file(&path);
                            return Err(upload_io(error));
                        }
                        return Ok(path);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(upload_io(error)),
                }
            }
            Err(error) => return Err(upload_io(error)),
        }
    }
    Err(ProjectError::UploadStorageIo {
        message: format!("too many legacy upload name collisions for '{file_name}'"),
    })
}

fn files_equal(left: &Path, right: &Path) -> Result<bool, ProjectError> {
    if std::fs::metadata(left).map_err(upload_io)?.len() != std::fs::metadata(right).map_err(upload_io)?.len() {
        return Ok(false);
    }
    let mut left = File::open(left).map_err(upload_io)?;
    let mut right = File::open(right).map_err(upload_io)?;
    let mut left_buffer = [0_u8; 8192];
    let mut right_buffer = [0_u8; 8192];
    loop {
        let left_read = left.read(&mut left_buffer).map_err(upload_io)?;
        let right_read = right.read(&mut right_buffer).map_err(upload_io)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn open_private_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn split_base_ext(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(index) if index > 0 => (name[..index].to_owned(), name[index..].to_owned()),
        _ => (name.to_owned(), String::new()),
    }
}

fn upload_io(error: std::io::Error) -> ProjectError {
    ProjectError::UploadStorageIo {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(root: &tempfile::TempDir, legacy: &tempfile::TempDir) -> ManagedUploadStore {
        ManagedUploadStore::for_test(root.path().join("uploads"), legacy.path().join("aionui"))
    }

    #[tokio::test]
    async fn durable_upload_is_user_scoped_collision_safe_and_restart_stable() {
        let root = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        let first_store = store(&root, &legacy);
        let first = first_store
            .create("user-a", "note.txt", b"one", Some("conv-a"))
            .await
            .unwrap();
        let second = first_store
            .create("user-a", "note.txt", b"two", Some("conv-a"))
            .await
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        drop(first_store);

        let restarted = store(&root, &legacy);
        assert_eq!(restarted.resolve("user-a", &first).await.unwrap(), first);
        assert!(matches!(
            restarted.resolve("user-b", &first).await,
            Err(ProjectError::UploadPathOutsideRoot { .. })
        ));
    }

    #[tokio::test]
    async fn legacy_general_adoption_requires_persisted_history_evidence_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        let legacy_file = legacy.path().join("aionui/general/old.txt");
        std::fs::create_dir_all(legacy_file.parent().unwrap()).unwrap();
        std::fs::write(&legacy_file, b"old").unwrap();
        let uploads = store(&root, &legacy);
        let collision = uploads
            .create("system_default_user", "old.txt", b"new", None)
            .await
            .unwrap();
        assert!(matches!(
            uploads
                .resolve("system_default_user", legacy_file.to_str().unwrap())
                .await,
            Err(ProjectError::LegacyUploadOwnerUnknown)
        ));
        let first = uploads
            .adopt_persisted("system_default_user", "conv-a", legacy_file.to_str().unwrap())
            .await
            .unwrap();
        let second = uploads
            .adopt_persisted("system_default_user", "conv-a", legacy_file.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_ne!(first, collision);
        assert_eq!(std::fs::read(first).unwrap(), b"old");
    }

    #[tokio::test]
    async fn rejects_traversal_and_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        let uploads = store(&root, &legacy);
        assert!(matches!(
            uploads.create("user-a", "../escape.txt", b"x", Some("conv-a")).await,
            Err(ProjectError::InvalidUploadName { .. })
        ));
        let managed = uploads
            .create("user-a", "safe.txt", b"safe", Some("conv-a"))
            .await
            .unwrap();
        let managed_path = Path::new(&managed);
        std::fs::remove_file(managed_path).unwrap();
        let outside = root.path().join("outside.txt");
        std::fs::write(&outside, b"outside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, managed_path).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside, managed_path).unwrap();
        assert!(matches!(
            uploads.resolve("user-a", &managed).await,
            Err(ProjectError::UploadPathOutsideRoot { .. })
        ));

        let legacy_link = legacy.path().join("aionui/general/link.txt");
        std::fs::create_dir_all(legacy_link.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &legacy_link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside, &legacy_link).unwrap();
        assert!(matches!(
            uploads
                .adopt_persisted("system_default_user", "conv-a", legacy_link.to_str().unwrap())
                .await,
            Err(ProjectError::UploadPathOutsideRoot { .. })
        ));

        let second_root = tempfile::tempdir().unwrap();
        let second_legacy = tempfile::tempdir().unwrap();
        let storage_root = second_root.path().join("uploads");
        std::fs::create_dir(&storage_root).unwrap();
        let escaped_users = second_root.path().join("escaped-users");
        std::fs::create_dir(&escaped_users).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&escaped_users, storage_root.join("users")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&escaped_users, storage_root.join("users")).unwrap();
        let poisoned = store(&second_root, &second_legacy);
        assert!(matches!(
            poisoned.create("user-a", "blocked.txt", b"x", Some("conv-a")).await,
            Err(ProjectError::UploadStorageIo { .. })
        ));
        assert!(std::fs::read_dir(&escaped_users).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn missing_managed_reference_remains_explicitly_missing() {
        let root = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        let uploads = store(&root, &legacy);
        let path = uploads
            .create("user-a", "gone.txt", b"gone", Some("conv-a"))
            .await
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            uploads.resolve("user-a", &path).await,
            Err(ProjectError::ChatFileMissing { .. })
        ));
    }
}
