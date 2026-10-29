//! Chunked uploads.
//!
//! The browser sends a file as a series of `PUT <path>?upload=<id>&offset=<n>` requests, one per
//! chunk, then `POST <path>?upload=<id>&size=<n>` to finish. Chunks are written to a hidden
//! temporary file next to the target, which is only moved into place once every byte has arrived.
//! `DELETE <path>?upload=<id>` abandons an upload.

use std::collections::HashSet;
use std::io::{ErrorKind, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use futures_util::TryStreamExt;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncSeekExt;
use tokio_util::io::StreamReader;
use walkdir::WalkDir;

use crate::error::Error;
use crate::paths;

const TEMP_PREFIX: &str = ".upserve-";
const TEMP_SUFFIX: &str = ".part";

/// Whether `name` is one of our temporary files, which are hidden from listings.
pub fn is_temp_file(name: &str) -> bool {
    name.starts_with(TEMP_PREFIX) && name.ends_with(TEMP_SUFFIX)
}

/// Deletes temporary files left under `root` by earlier runs, returning how many were removed.
///
/// Only files last written before `started` are touched, so this can run alongside new uploads.
pub fn remove_stale(root: &Path, started: SystemTime) -> usize {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.file_name().to_str().is_some_and(is_temp_file))
        .filter(|entry| {
            let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
            modified.is_some_and(|modified| modified < started)
        })
        .filter(|entry| std::fs::remove_file(entry.path()).is_ok())
        .count()
}

/// Handles uploads into the served directory.
pub struct Uploads {
    /// Canonical path of the served directory.
    root: PathBuf,
    allow_replace: bool,
    /// Temporary files of uploads that haven't finished, so they can be removed on exit.
    unfinished: Mutex<HashSet<PathBuf>>,
}

impl Uploads {
    pub fn new(root: PathBuf, allow_replace: bool) -> Self {
        Self {
            root,
            allow_replace,
            unfinished: Mutex::default(),
        }
    }

    /// Writes one chunk of an upload at `offset`.
    ///
    /// Chunks must arrive in order, but resending a chunk is fine: anything after `offset` is
    /// discarded first, so a retried chunk simply overwrites its earlier attempt.
    pub async fn write_chunk(
        &self,
        target: &Path,
        id: &str,
        offset: u64,
        body: Body,
    ) -> Result<(), Error> {
        let upload = Upload::new(&self.root, target, id)?;
        if offset == 0 {
            // Fail early rather than after the whole file has been sent.
            check_replaceable(&upload.target, self.allow_replace).await?;
            paths::create_dir_within(&self.root, upload.dir()).await?;
            self.unfinished.lock().unwrap().insert(upload.temp.clone());
        } else {
            paths::ensure_within(&self.root, upload.dir()).await?;
        }

        let mut file = OpenOptions::new()
            .write(true)
            .create(offset == 0)
            .open(&upload.temp)
            .await
            .map_err(not_found_as_unknown_upload)?;
        if offset > file.metadata().await?.len() {
            return Err(Error::conflict("chunk arrived out of order"));
        }
        file.set_len(offset).await?;
        file.seek(SeekFrom::Start(offset)).await?;

        // A failed read means the client went away, e.g. because the upload was cancelled.
        let body = body
            .into_data_stream()
            .map_err(|err| std::io::Error::new(ErrorKind::ConnectionAborted, err));
        let mut body = StreamReader::new(body);
        tokio::io::copy(&mut body, &mut file).await?;
        Ok(())
    }

    /// Checks that all `size` bytes have arrived and moves the upload into place, setting its
    /// modification time to `mtime` (milliseconds since the Unix epoch) if given.
    pub async fn finish(
        &self,
        target: &Path,
        id: &str,
        size: u64,
        mtime: Option<u64>,
    ) -> Result<(), Error> {
        let upload = Upload::new(&self.root, target, id)?;
        paths::ensure_within(&self.root, upload.dir()).await?;

        let file = OpenOptions::new()
            .write(true)
            .open(&upload.temp)
            .await
            .map_err(not_found_as_unknown_upload)?;
        let received = file.metadata().await?.len();
        if received != size {
            return Err(Error::conflict(format!(
                "expected {size} bytes but received {received}"
            )));
        }

        place(&upload.temp, &upload.target, self.allow_replace).await?;
        self.unfinished.lock().unwrap().remove(&upload.temp);
        // Set only once the file is in place, so `remove_stale` can't mistake it for an old
        // temporary file. The handle still refers to the same file after the move. Not every
        // filesystem allows this, and the upload has succeeded either way.
        if let Some(ms) = mtime {
            let modified = UNIX_EPOCH + Duration::from_millis(ms);
            if let Err(err) = file.into_std().await.set_modified(modified) {
                tracing::warn!(
                    "can't set modification time of {}: {err}",
                    upload.target.display()
                );
            }
        }
        Ok(())
    }

    /// Abandons an upload, deleting whatever has been received so far.
    pub async fn abort(&self, target: &Path, id: &str) -> Result<(), Error> {
        let upload = Upload::new(&self.root, target, id)?;
        paths::ensure_within(&self.root, upload.dir()).await?;
        self.unfinished.lock().unwrap().remove(&upload.temp);
        match fs::remove_file(&upload.temp).await {
            Err(err) if err.kind() != ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        }
    }

    /// Deletes the temporary files of uploads that never finished, returning how many there were.
    pub async fn remove_unfinished(&self) -> usize {
        let unfinished = std::mem::take(&mut *self.unfinished.lock().unwrap());
        let mut removed = 0;
        for temp in unfinished {
            if fs::remove_file(temp).await.is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

/// Where an upload is going and where its data lives until then.
struct Upload {
    target: PathBuf,
    temp: PathBuf,
}

impl Upload {
    fn new(root: &Path, target: &Path, id: &str) -> Result<Self, Error> {
        if !(16..=64).contains(&id.len()) || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(Error::bad_request("invalid upload id"));
        }
        if target == root {
            return Err(Error::bad_request("missing file name"));
        }
        let name = target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if is_temp_file(name) {
            return Err(Error::bad_request("that file name is reserved"));
        }
        Ok(Self {
            target: target.to_path_buf(),
            temp: target.with_file_name(format!("{TEMP_PREFIX}{id}{TEMP_SUFFIX}")),
        })
    }

    fn dir(&self) -> &Path {
        self.target.parent().expect("target is below root")
    }
}

/// Moves a finished upload to `target`, never replacing a folder, and only replacing a file
/// if `allow_replace` is set.
async fn place(temp: &Path, target: &Path, allow_replace: bool) -> Result<(), Error> {
    check_replaceable(target, allow_replace).await?;
    if !allow_replace {
        // Linking fails if the target has appeared since the check, so a file uploaded under
        // the same name at the same time can't be clobbered.
        match fs::hard_link(temp, target).await {
            Ok(()) => return Ok(fs::remove_file(temp).await?),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                return Err(Error::conflict("a file with that name already exists"));
            }
            // The filesystem doesn't support hard links (FAT, some network shares).
            Err(_) => {}
        }
    }
    Ok(fs::rename(temp, target).await?)
}

async fn check_replaceable(target: &Path, allow_replace: bool) -> Result<(), Error> {
    match fs::symlink_metadata(target).await {
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
        Ok(meta) if meta.is_dir() => Err(Error::conflict("a folder with that name already exists")),
        Ok(_) if allow_replace => Ok(()),
        Ok(_) => Err(Error::conflict("a file with that name already exists")),
    }
}

fn not_found_as_unknown_upload(err: std::io::Error) -> Error {
    if err.kind() == ErrorKind::NotFound {
        Error::new(axum::http::StatusCode::NOT_FOUND, "unknown upload")
    } else {
        err.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef";

    fn root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    async fn upload(uploads: &Uploads, path: &str, chunks: &[&str]) -> Result<(), Error> {
        let target = paths::resolve(&uploads.root, path)?;
        let mut offset = 0;
        for chunk in chunks {
            let body = Body::from(chunk.to_string());
            uploads.write_chunk(&target, ID, offset, body).await?;
            offset += chunk.len() as u64;
        }
        uploads.finish(&target, ID, offset, None).await
    }

    fn temp_files(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| is_temp_file(e.as_ref().unwrap().file_name().to_str().unwrap()))
            .count()
    }

    #[tokio::test]
    async fn uploads_in_chunks_into_new_folders() {
        let (_dir, root) = root();
        let uploads = Uploads::new(root.clone(), false);
        upload(&uploads, "/a/b/file.txt", &["hello ", "world"])
            .await
            .unwrap();
        let content = std::fs::read_to_string(root.join("a/b/file.txt")).unwrap();
        assert_eq!(content, "hello world");
        assert_eq!(temp_files(&root.join("a/b")), 0);
        assert_eq!(uploads.remove_unfinished().await, 0);
    }

    #[tokio::test]
    async fn keeps_modification_time() {
        let (_dir, root) = root();
        let uploads = Uploads::new(root.clone(), false);
        let target = root.join("f");
        uploads
            .write_chunk(&target, ID, 0, Body::from("x"))
            .await
            .unwrap();
        uploads
            .finish(&target, ID, 1, Some(1_700_000_000_000))
            .await
            .unwrap();
        let modified = std::fs::metadata(&target).unwrap().modified().unwrap();
        assert_eq!(modified, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    }

    #[tokio::test]
    async fn retried_chunk_overwrites_previous_attempt() {
        let (_dir, root) = root();
        let uploads = Uploads::new(root.clone(), false);
        let target = root.join("f");
        for (offset, chunk) in [(0, "aaaa"), (2, "garbage"), (2, "bb")] {
            let body = Body::from(chunk);
            uploads
                .write_chunk(&target, ID, offset, body)
                .await
                .unwrap();
        }
        uploads.finish(&target, ID, 4, None).await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "aabb");
    }

    #[tokio::test]
    async fn rejects_gaps_and_short_uploads() {
        let (_dir, root) = root();
        let uploads = Uploads::new(root.clone(), false);
        let target = root.join("f");
        uploads
            .write_chunk(&target, ID, 0, Body::from("aa"))
            .await
            .unwrap();
        let gap = uploads.write_chunk(&target, ID, 5, Body::from("bb")).await;
        assert_eq!(gap.unwrap_err().status(), 409);
        let short = uploads.finish(&target, ID, 10, None).await;
        assert_eq!(short.unwrap_err().status(), 409);
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn replaces_only_when_allowed() {
        let (_dir, root) = root();
        let (careful, careless) = (
            Uploads::new(root.clone(), false),
            Uploads::new(root.clone(), true),
        );
        upload(&careful, "/f", &["one"]).await.unwrap();
        assert_eq!(
            upload(&careful, "/f", &["two"]).await.unwrap_err().status(),
            409
        );
        upload(&careless, "/f", &["three"]).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.join("f")).unwrap(), "three");
    }

    #[tokio::test]
    async fn never_replaces_folders() {
        let (_dir, root) = root();
        std::fs::create_dir(root.join("d")).unwrap();
        let uploads = Uploads::new(root, true);
        assert_eq!(
            upload(&uploads, "/d", &["x"]).await.unwrap_err().status(),
            409
        );
    }

    #[tokio::test]
    async fn rejects_reserved_names_and_bad_ids() {
        let (_dir, root) = root();
        let uploads = Uploads::new(root.clone(), false);
        let reserved = root.join(format!("{TEMP_PREFIX}{ID}{TEMP_SUFFIX}"));
        for (target, id) in [(&reserved, ID), (&root.join("f"), "../x"), (&root, ID)] {
            let result = uploads.write_chunk(target, id, 0, Body::empty()).await;
            assert!(
                result.is_err(),
                "{} with id {id} should be rejected",
                target.display()
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_follow_symlinks_out_of_root() {
        let (_dir, root) = root();
        let (_outside_dir, outside) = self::root();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let uploads = Uploads::new(root, false);
        let err = upload(&uploads, "/link/new/f", &["x"]).await.unwrap_err();
        assert_eq!(err.status(), 403);
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn removes_unfinished_uploads() {
        let (_dir, root) = root();
        let uploads = Uploads::new(root.clone(), false);
        let target = root.join("sub/f");
        uploads
            .write_chunk(&target, ID, 0, Body::from("half"))
            .await
            .unwrap();
        assert_eq!(temp_files(&root.join("sub")), 1);
        assert_eq!(uploads.remove_unfinished().await, 1);
        assert_eq!(temp_files(&root.join("sub")), 0);
    }

    #[test]
    fn removes_only_stale_temp_files() {
        let (_dir, root) = root();
        std::fs::create_dir(root.join("sub")).unwrap();
        let stale = root.join(format!("sub/{TEMP_PREFIX}{ID}{TEMP_SUFFIX}"));
        let fresh = root.join(format!("{TEMP_PREFIX}fresh{ID}{TEMP_SUFFIX}"));
        let ordinary = root.join("sub/notes.part");
        let started = SystemTime::now();
        let hour = Duration::from_secs(3600);
        for (path, modified) in [
            (&stale, started - hour),
            (&fresh, started + hour),
            (&ordinary, started - hour),
        ] {
            std::fs::write(path, "x").unwrap();
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(modified).unwrap();
        }

        assert_eq!(remove_stale(&root, started), 1);
        assert!(!stale.exists());
        assert!(fresh.exists() && ordinary.exists());
    }
}
