//! Open only real files below a registered root. On Unix bind operations to an
//! open directory descriptor; on Windows keep every directory pinned against
//! deletion/replacement while resolving its descendants.
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum SourceEditError {
    #[error("The source changed")]
    Conflict,
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub struct ProjectSourceFile {
    #[cfg(unix)]
    parent: File,
    #[cfg(unix)]
    root: PathBuf,
    #[cfg(windows)]
    _parents: Vec<File>,
    path: PathBuf,
}

impl ProjectSourceFile {
    pub fn open(root: &Path, path: &Path) -> io::Result<Self> {
        if !root.is_absolute() || !path.is_absolute() {
            return Err(io::Error::other("Source paths must be absolute"));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("unregistered source"))?;
        if relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(io::Error::other("invalid source name"));
        }
        let parent_path = path.parent().ok_or_else(|| io::Error::other("missing source parent"))?;
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags, open, openat};
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let mut parent = open("/", flags, Mode::empty())?;
            for component in parent_path.components() {
                if let std::path::Component::Normal(name) = component {
                    parent = openat(&parent, name, flags, Mode::empty())?;
                }
            }
            Ok(Self {
                parent: parent.into(),
                root: root.to_owned(),
                path: path.to_owned(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let mut dirs = Vec::new();
            for p in parent_path.ancestors().collect::<Vec<_>>().into_iter().rev() {
                let dir = fs::OpenOptions::new()
                    .read(true)
                    .share_mode(3)
                    .custom_flags(0x0200_0000 | 0x0020_0000)
                    .open(p)?;
                if super::is_link(&dir.metadata()?) {
                    return Err(io::Error::other("linked directory"));
                }
                dirs.push(dir);
            }
            Ok(Self {
                _parents: dirs,
                path: path.to_owned(),
            })
        }
    }

    pub fn read(&self, max_bytes: usize) -> io::Result<(String, fs::Permissions)> {
        #[cfg(unix)]
        let file: File = {
            use rustix::fs::{Mode, OFlags, openat};
            openat(
                &self.parent,
                self.path.file_name().unwrap(),
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )?
            .into()
        };
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(0x0020_0000)
                .open(&self.path)?
        };
        let meta = file.metadata()?;
        if !meta.is_file() || super::is_link(&meta) || meta.len() > max_bytes as u64 {
            return Err(io::Error::other("invalid or oversized rule source"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.nlink() != 1 {
                return Err(io::Error::other("hardlinked rule source"));
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx,
            };
            let mut info = FILE_STANDARD_INFO::default();
            // The output buffer is initialized, correctly sized and lives for
            // the entire synchronous call. The open file owns the OS handle.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileStandardInfo,
                    (&mut info as *mut FILE_STANDARD_INFO).cast(),
                    std::mem::size_of_val(&info) as u32,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if info.NumberOfLinks != 1 {
                return Err(io::Error::other("hardlinked rule source"));
            }
        }
        let mut raw = String::new();
        file.take(max_bytes as u64 + 1).read_to_string(&mut raw)?;
        if raw.len() > max_bytes || raw.contains('\0') {
            return Err(io::Error::other("invalid rule text"));
        }
        Ok((raw, meta.permissions()))
    }

    pub fn replace(
        &self,
        revision: &str,
        edited: &str,
        permissions: fs::Permissions,
        max_bytes: usize,
    ) -> Result<(), SourceEditError> {
        let write_error = SourceEditError::Io;
        #[cfg(unix)]
        {
            use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
            // Serialize cooperating app instances in the same directory without
            // creating persistent lock files in a user's repository.
            self.parent.lock().map_err(write_error)?;
            let _lock = DirectoryLock(&self.parent);
            let name = format!(".ellmsha-rule-{}.tmp", crate::generate_id());
            let file = openat(
                &self.parent,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|e| write_error(e.into()))?;
            let result = (|| {
                let mut file = File::from(file);
                file.write_all(edited.as_bytes()).map_err(write_error)?;
                file.set_permissions(permissions).map_err(write_error)?;
                file.sync_all().map_err(write_error)?;
                self.verify(revision, max_bytes)?;
                renameat(
                    &self.parent,
                    name.as_str(),
                    &self.parent,
                    self.path.file_name().unwrap(),
                )
                .map_err(|e| write_error(e.into()))?;
                Ok(())
            })();
            if result.is_err() {
                let _ = unlinkat(&self.parent, name.as_str(), AtFlags::empty());
            }
            result
        }
        #[cfg(windows)]
        {
            let mut tmp = tempfile::NamedTempFile::new_in(self.path.parent().unwrap()).map_err(write_error)?;
            tmp.write_all(edited.as_bytes()).map_err(write_error)?;
            tmp.as_file().set_permissions(permissions).map_err(write_error)?;
            tmp.as_file().sync_all().map_err(write_error)?;
            self.verify(revision, max_bytes)?;
            tmp.persist(&self.path).map_err(|err| write_error(err.error))?;
            Ok(())
        }
    }

    fn verify(&self, revision: &str, max_bytes: usize) -> Result<(), SourceEditError> {
        use sha2::{Digest, Sha256};
        let conflict = || SourceEditError::Conflict;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let fresh = Self::open(&self.root, &self.path).map_err(|_| conflict())?;
            let new = fresh.parent.metadata().map_err(|_| conflict())?;
            let old = self.parent.metadata().map_err(|_| conflict())?;
            if (old.dev(), old.ino()) != (new.dev(), new.ino()) {
                return Err(conflict());
            }
        }

        let (current, _) = self.read(max_bytes).map_err(|_| conflict())?;
        if format!("{:x}", Sha256::digest(current.as_bytes())) != revision {
            return Err(conflict());
        }
        Ok(())
    }
}

#[cfg(unix)]
struct DirectoryLock<'a>(&'a File);
#[cfg(unix)]
impl Drop for DirectoryLock<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    #[test]
    fn replaced_revision_is_visible_and_a_stale_writer_cannot_overwrite_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("AGENTS.md");
        fs::write(&path, "one").unwrap();
        let a = ProjectSourceFile::open(&root, &path).unwrap();
        let b = ProjectSourceFile::open(&root, &path).unwrap();
        let (one, mode) = a.read(32).unwrap();
        let rev = format!("{:x}", Sha256::digest(one.as_bytes()));
        a.replace(&rev, "two", mode.clone(), 32).unwrap();
        assert!(matches!(
            b.replace(&rev, "three", mode, 32),
            Err(SourceEditError::Conflict)
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_moved_outside_the_project_cannot_be_written_through_an_old_handle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let detached = tempfile::tempdir().unwrap();
        let original = root.join("client");
        fs::create_dir(&original).unwrap();
        let path = original.join("AGENTS.md");
        fs::write(&path, "before").unwrap();
        let source = ProjectSourceFile::open(&root, &path).unwrap();
        let (raw, mode) = source.read(32).unwrap();
        let moved = detached.path().join("moved");
        fs::rename(&original, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &original).unwrap();
        assert!(matches!(
            source.replace(&format!("{:x}", Sha256::digest(raw.as_bytes())), "outside", mode, 32),
            Err(SourceEditError::Conflict)
        ));
        assert_eq!(fs::read_to_string(moved.join("AGENTS.md")).unwrap(), "before");
    }
}
