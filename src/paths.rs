//! Mapping URL paths onto the served directory, without letting requests escape it.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use axum::http::StatusCode;
use percent_encoding::percent_decode_str;
use tokio::fs;

use crate::error::Error;

/// Resolves a percent-encoded URL path to a path under `root`.
///
/// Every segment must be a plain file name, so `..`, Windows drive prefixes and the like are
/// rejected rather than interpreted.
pub fn resolve(root: &Path, url_path: &str) -> Result<PathBuf, Error> {
    let decoded = percent_decode_str(url_path)
        .decode_utf8()
        .map_err(|_| Error::bad_request("path is not valid UTF-8"))?;

    let mut path = root.to_path_buf();
    for segment in decoded.split('/').filter(|s| !s.is_empty()) {
        let mut components = Path::new(segment).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) if !segment.contains(['\\', '\0']) => {
                path.push(name)
            }
            _ => return Err(Error::bad_request("invalid path")),
        }
    }
    Ok(path)
}

/// Checks that `dir` is inside `root` once symlinks are resolved.
///
/// `root` must already be canonical.
pub async fn ensure_within(root: &Path, dir: &Path) -> Result<(), Error> {
    if fs::canonicalize(dir).await?.starts_with(root) {
        Ok(())
    } else {
        Err(Error::forbidden("path leads outside the served directory"))
    }
}

/// Creates `dir` and any missing parents, one level at a time, refusing to follow a symlink
/// out of `root`.
///
/// `root` must already be canonical and `dir` must come from [`resolve`].
pub async fn create_dir_within(root: &Path, dir: &Path) -> Result<(), Error> {
    let relative = dir
        .strip_prefix(root)
        .map_err(|_| Error::new(StatusCode::INTERNAL_SERVER_ERROR, "path is not under root"))?;

    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match fs::create_dir(&path).await {
            Err(err) if err.kind() != ErrorKind::AlreadyExists => return Err(err.into()),
            _ => ensure_within(root, &path).await?,
        }
        if !fs::metadata(&path).await?.is_dir() {
            return Err(Error::conflict(format!(
                "{} is a file, not a folder",
                component.as_os_str().to_string_lossy()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_plain_paths() {
        let root = Path::new("/srv");
        assert_eq!(resolve(root, "/").unwrap(), root);
        assert_eq!(
            resolve(root, "/a/b.txt").unwrap(),
            root.join("a").join("b.txt")
        );
        assert_eq!(resolve(root, "//a//b/").unwrap(), root.join("a").join("b"));
        assert_eq!(
            resolve(root, "/a%20b/%C3%A9").unwrap(),
            root.join("a b").join("é")
        );
    }

    #[test]
    fn rejects_traversal() {
        let root = Path::new("/srv");
        for path in [
            "/..",
            "/a/../../etc",
            "/%2e%2e/etc",
            "/.",
            "/a%2f..%2f..",
            "/a%5c..",
            "/a%00b",
        ] {
            assert!(resolve(root, path).is_err(), "{path} should be rejected");
        }
    }
}
