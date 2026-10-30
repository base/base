use std::{
    io,
    path::{Path, PathBuf},
};

/// Checked paths beneath a recorder-owned local root.
#[derive(Debug)]
pub struct LocalPath;

impl LocalPath {
    /// Walk every key component without accepting symlinks, creating parent directories if requested.
    pub async fn resolve(root: &Path, key: &str, create: bool) -> io::Result<PathBuf> {
        if key.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty object key"));
        }
        if create {
            tokio::fs::create_dir_all(root).await?;
        }
        let mut path = root.to_owned();
        let mut parts = key.split('/').peekable();
        while let Some(part) = parts.next() {
            if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "object key component"));
            }
            path.push(part);
            if create && parts.peek().is_some() {
                match tokio::fs::create_dir(&path).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
            match tokio::fs::symlink_metadata(&path).await {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "symlink below storage root",
                    ));
                }
                Ok(_) => {}
                Err(e)
                    if create && parts.peek().is_none() && e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(path)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, io::ErrorKind, os::unix::fs::symlink, path::PathBuf};

    use futures::TryStreamExt;
    use tempfile::TempDir;

    use crate::{ReplayStore, StoreError};

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

            assert!(
                matches!(result, Err(StoreError::Io(e)) if e.kind() == ErrorKind::InvalidInput),
                "depth {depth}"
            );
        }
    }

    #[tokio::test]
    async fn writes_reject_symlinks_at_every_depth() {
        for depth in 1..=6 {
            let fixture = SymlinkFixture::new(depth);
            let store = ReplayStore::Local(fixture.root.path().to_owned());

            let result = store.put(fixture.key, vec![1]).await;

            assert!(
                matches!(result, Err(StoreError::Io(e)) if e.kind() == ErrorKind::InvalidInput),
                "depth {depth}"
            );
            assert!(!fixture.outside_object.exists());
        }
    }
}
