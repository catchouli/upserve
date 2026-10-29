//! Chunked uploads.
//!
//! The browser sends a file as a series of `PUT <path>?upload=<id>&offset=<n>` requests, one per
//! chunk, then `POST <path>?upload=<id>&size=<n>` to finish. Chunks are written to a hidden
//! temporary file next to the target, which is only moved into place once every byte has arrived.
//! `DELETE <path>?upload=<id>` abandons an upload.

use std::io::{ErrorKind, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use axum::body::Body;
use futures_util::TryStreamExt;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncSeekExt;
use tokio_util::io::StreamReader;

use crate::error::Error;
use crate::paths;

const TEMP_PREFIX: &str = ".upserve-";
const TEMP_SUFFIX: &str = ".part";

/// Whether `name` is one of our temporary files, which are hidden from listings.
pub fn is_temp_file(name: &str) -> bool {
    name.starts_with(TEMP_PREFIX) && name.ends_with(TEMP_SUFFIX)
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
        let name = target.file_name().and_then(|n| n.to_str()).unwrap_or_default();
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

/// Writes one chunk of an upload at `offset`.
///
/// Chunks must arrive in order, but resending a chunk is fine: anything after `offset` is
/// discarded first, so a retried chunk simply overwrites its earlier attempt.
pub async fn write_chunk(
    root: &Path,
    target: &Path,
    id: &str,
    offset: u64,
    body: Body,
    allow_replace: bool,
) -> Result<(), Error> {
    let upload = Upload::new(root, target, id)?;
    if offset == 0 {
        // Fail early rather than after the whole file has been sent.
        check_replaceable(&upload.target, allow_replace).await?;
        paths::create_dir_within(root, upload.dir()).await?;
    } else {
        paths::ensure_within(root, upload.dir()).await?;
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

    let mut body = StreamReader::new(body.into_data_stream().map_err(std::io::Error::other));
    tokio::io::copy(&mut body, &mut file).await?;
    Ok(())
}

/// Checks that all `size` bytes have arrived and moves the upload into place, setting its
/// modification time to `mtime` (milliseconds since the Unix epoch) if given.
pub async fn finish(
    root: &Path,
    target: &Path,
    id: &str,
    size: u64,
    mtime: Option<u64>,
    allow_replace: bool,
) -> Result<(), Error> {
    let upload = Upload::new(root, target, id)?;
    paths::ensure_within(root, upload.dir()).await?;

    let file = OpenOptions::new()
        .write(true)
        .open(&upload.temp)
        .await
        .map_err(not_found_as_unknown_upload)?;
    let received = file.metadata().await?.len();
    if received != size {
        return Err(Error::conflict(format!("expected {size} bytes but received {received}")));
    }
    if let Some(ms) = mtime {
        file.into_std().await.set_modified(UNIX_EPOCH + Duration::from_millis(ms))?;
    }

    place(&upload.temp, &upload.target, allow_replace).await
}

/// Abandons an upload, deleting whatever has been received so far.
pub async fn abort(root: &Path, target: &Path, id: &str) -> Result<(), Error> {
    let upload = Upload::new(root, target, id)?;
    paths::ensure_within(root, upload.dir()).await?;
    match fs::remove_file(&upload.temp).await {
        Err(err) if err.kind() != ErrorKind::NotFound => Err(err.into()),
        _ => Ok(()),
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

    async fn upload(root: &Path, path: &str, chunks: &[&str], allow_replace: bool) -> Result<(), Error> {
        let target = paths::resolve(root, path)?;
        let mut offset = 0;
        for chunk in chunks {
            write_chunk(root, &target, ID, offset, Body::from(chunk.to_string()), allow_replace).await?;
            offset += chunk.len() as u64;
        }
        finish(root, &target, ID, offset, None, allow_replace).await
    }

    #[tokio::test]
    async fn uploads_in_chunks_into_new_folders() {
        let (_dir, root) = root();
        upload(&root, "/a/b/file.txt", &["hello ", "world"], false).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a/b/file.txt")).unwrap(), "hello world");
        assert_eq!(std::fs::read_dir(root.join("a/b")).unwrap().count(), 1, "temp file left behind");
    }

    #[tokio::test]
    async fn retried_chunk_overwrites_previous_attempt() {
        let (_dir, root) = root();
        let target = root.join("f");
        write_chunk(&root, &target, ID, 0, Body::from("aaaa"), false).await.unwrap();
        write_chunk(&root, &target, ID, 2, Body::from("garbage"), false).await.unwrap();
        write_chunk(&root, &target, ID, 2, Body::from("bb"), false).await.unwrap();
        finish(&root, &target, ID, 4, None, false).await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "aabb");
    }

    #[tokio::test]
    async fn rejects_gaps_and_short_uploads() {
        let (_dir, root) = root();
        let target = root.join("f");
        write_chunk(&root, &target, ID, 0, Body::from("aa"), false).await.unwrap();
        let gap = write_chunk(&root, &target, ID, 5, Body::from("bb"), false).await;
        assert_eq!(gap.unwrap_err().status(), 409);
        let short = finish(&root, &target, ID, 10, None, false).await;
        assert_eq!(short.unwrap_err().status(), 409);
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn replaces_only_when_allowed() {
        let (_dir, root) = root();
        upload(&root, "/f", &["one"], false).await.unwrap();
        assert_eq!(upload(&root, "/f", &["two"], false).await.unwrap_err().status(), 409);
        upload(&root, "/f", &["three"], true).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.join("f")).unwrap(), "three");
    }

    #[tokio::test]
    async fn never_replaces_folders() {
        let (_dir, root) = root();
        std::fs::create_dir(root.join("d")).unwrap();
        assert_eq!(upload(&root, "/d", &["x"], true).await.unwrap_err().status(), 409);
    }

    #[tokio::test]
    async fn rejects_reserved_names_and_bad_ids() {
        let (_dir, root) = root();
        let reserved = root.join(format!("{TEMP_PREFIX}{ID}{TEMP_SUFFIX}"));
        assert!(write_chunk(&root, &reserved, ID, 0, Body::empty(), false).await.is_err());
        assert!(write_chunk(&root, &root.join("f"), "../x", 0, Body::empty(), false).await.is_err());
        assert!(write_chunk(&root, &root, ID, 0, Body::empty(), false).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_follow_symlinks_out_of_root() {
        let (_dir, root) = root();
        let (_outside_dir, outside) = self::root();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let err = upload(&root, "/link/new/f", &["x"], false).await.unwrap_err();
        assert_eq!(err.status(), 403);
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }
}
