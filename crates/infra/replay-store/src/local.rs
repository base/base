use std::{
    fs::{self, File},
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::{
    fd::OwnedFd,
    fs::{AtFlags, Mode, OFlags, linkat, mkdirat, open, openat, unlinkat},
    io::Errno,
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Object name relative to an opened, no-follow parent directory.
#[derive(Debug)]
pub struct LocalObject {
    parent: OwnedFd,
    name: String,
}

impl LocalObject {
    /// Pin the parent directory, walking each key component with no-follow directory opens.
    pub fn open(root: &Path, key: &str, create: bool) -> io::Result<Self> {
        if key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".." || part.contains('\\'))
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "object key component"));
        }
        if create {
            fs::create_dir_all(root)?;
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut parent = open(root, flags, Mode::empty())?;
        let (directories, name) = key.rsplit_once('/').unwrap_or(("", key));
        for part in directories.split('/').filter(|p| !p.is_empty()) {
            if create {
                match mkdirat(&parent, part, Mode::RWXU) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            parent = openat(&parent, part, flags, Mode::empty())?;
        }
        Ok(Self { parent, name: name.to_owned() })
    }

    /// Open the final object without following a replacement symlink.
    pub fn read(&self) -> io::Result<File> {
        Ok(File::from(openat(
            &self.parent,
            self.name.as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?))
    }

    /// Atomically link a complete temporary file into place without replacing an existing object.
    pub fn publish(&self, bytes: &[u8]) -> io::Result<()> {
        let mut temp = LocalTemp::new(&self.parent)?;
        temp.file.write_all(bytes)?;
        linkat(
            &self.parent,
            temp.name.as_str(),
            &self.parent,
            self.name.as_str(),
            AtFlags::empty(),
        )?;
        Ok(())
    }
}

/// Directory-relative temporary file, unlinked on drop after success or failure.
#[derive(Debug)]
pub struct LocalTemp<'a> {
    parent: &'a OwnedFd,
    name: String,
    file: File,
}

impl<'a> LocalTemp<'a> {
    /// Exclusively create a private temporary file in the pinned directory.
    pub fn new(parent: &'a OwnedFd) -> io::Result<Self> {
        const MAX_TEMP_ATTEMPTS: usize = 128;
        for _ in 0..MAX_TEMP_ATTEMPTS {
            let name = format!(
                ".replay-{}-{}.tmp",
                std::process::id(),
                NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
            );
            match openat(
                parent,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            ) {
                Ok(fd) => return Ok(Self { parent, name, file: File::from(fd) }),
                Err(Errno::EXIST) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "temporary name collisions"))
    }
}

impl Drop for LocalTemp<'_> {
    fn drop(&mut self) {
        let _ = unlinkat(self.parent, self.name.as_str(), AtFlags::empty());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, io::Read, os::unix::fs::symlink, path::PathBuf};

    use futures::TryStreamExt;
    use tempfile::TempDir;

    use crate::{LocalObject, ReplayStore, StoreError};

    struct SymlinkFixture {
        root: TempDir,
        _outside: TempDir,
        key: String,
        outside_object: PathBuf,
    }

    impl SymlinkFixture {
        fn new(depth: usize) -> Self {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let key = ReplayStore::key("base", 0).unwrap();
            let parts = key.split('/').collect::<Vec<_>>();
            let link = root.path().join(parts[..depth].join("/"));
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            let target = outside.path().join("target");
            if depth < parts.len() {
                fs::create_dir(&target).unwrap();
            }
            symlink(&target, link).unwrap();
            let outside_object =
                if depth == parts.len() { target } else { target.join(parts[depth..].join("/")) };
            Self { root, _outside: outside, key, outside_object }
        }
    }

    #[tokio::test]
    async fn reads_reject_symlinks_at_every_depth() {
        for depth in 1..=6 {
            let fixture = SymlinkFixture::new(depth);
            fs::create_dir_all(fixture.outside_object.parent().unwrap()).unwrap();
            let line = r#"{"stream":"builder_ingress","received_at_ms":0,"method":"call","params":[],"source":"x"}"#;
            fs::write(&fixture.outside_object, zstd::encode_all(line.as_bytes(), 0).unwrap())
                .unwrap();
            let store = ReplayStore::Local(fixture.root.path().to_owned());

            let result: Result<Vec<_>, _> = store.read("base", 0..=0).try_collect().await;

            assert!(matches!(result, Err(StoreError::Io(_))), "depth {depth}");
        }
    }

    #[tokio::test]
    async fn writes_reject_symlinks_at_every_depth() {
        for depth in 1..=6 {
            let fixture = SymlinkFixture::new(depth);
            let store = ReplayStore::Local(fixture.root.path().to_owned());

            let result = store.put(fixture.key, vec![1]).await;

            assert!(matches!(result, Err(StoreError::Io(_))), "depth {depth}");
            assert!(!fixture.outside_object.exists());
        }
    }

    #[test]
    fn replacement_after_parent_open_cannot_follow_final_symlink() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(root.path().join("object"), b"inside").unwrap();
        fs::write(outside.path(), b"outside").unwrap();
        let object = LocalObject::open(root.path(), "object", false).unwrap();

        fs::remove_file(root.path().join("object")).unwrap();
        symlink(outside.path(), root.path().join("object")).unwrap();
        let result = object.read();

        assert!(result.is_err(), "replacement escaped: {result:?}");
    }

    #[test]
    fn replacement_after_parent_open_keeps_original_read_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("parent")).unwrap();
        fs::write(root.path().join("parent/object"), b"inside").unwrap();
        fs::write(outside.path().join("object"), b"outside").unwrap();
        let object = LocalObject::open(root.path(), "parent/object", false).unwrap();

        fs::rename(root.path().join("parent"), root.path().join("held")).unwrap();
        symlink(outside.path(), root.path().join("parent")).unwrap();
        let mut bytes = Vec::new();
        object.read().unwrap().read_to_end(&mut bytes).unwrap();

        assert_eq!(bytes, b"inside");
    }

    #[test]
    fn replacement_after_parent_open_keeps_original_publish_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let object = LocalObject::open(root.path(), "parent/object", true).unwrap();

        fs::rename(root.path().join("parent"), root.path().join("held")).unwrap();
        symlink(outside.path(), root.path().join("parent")).unwrap();
        object.publish(b"inside").unwrap();

        assert_eq!(fs::read(root.path().join("held/object")).unwrap(), b"inside");
        assert!(!outside.path().join("object").exists());
        assert_eq!(fs::read_dir(root.path().join("held")).unwrap().count(), 1);
    }
}
