//! The directory listing page, which also hosts the upload controls.

use std::path::Path;

use maud::{DOCTYPE, Markup, PreEscaped, html};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use tokio::fs;

use crate::error::Error;
use crate::upload;

/// Characters that are percent-encoded in a link to a single path segment.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

const SCRIPT: &str = include_str!("../assets/upload.js");
const STYLE: &str = include_str!("../assets/style.css");

struct Entry {
    name: String,
    is_dir: bool,
    is_symlink: bool,
}

pub async fn render(dir: &Path, url_path: &str) -> Result<Markup, Error> {
    let mut entries = Vec::new();
    let mut read_dir = fs::read_dir(dir).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if upload::is_temp_file(&name) {
            continue;
        }
        entries.push(Entry {
            is_dir: fs::metadata(entry.path()).await.is_ok_and(|m| m.is_dir()),
            is_symlink: entry.file_type().await?.is_symlink(),
            name,
        });
    }
    entries.sort_by_cached_key(|e| e.name.to_lowercase());

    let title = format!("Directory listing for {}", percent_decode_str(url_path).decode_utf8_lossy());
    Ok(html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                h1 { (title) }
                div #upload {
                    input #files type="file" multiple hidden;
                    input #folder type="file" webkitdirectory hidden;
                    button #pick-files type="button" { "Choose files…" }
                    " "
                    button #pick-folder type="button" { "Choose folder…" }
                    " "
                    button #start type="button" disabled { "Upload" }
                    " "
                    button #cancel type="button" hidden { "Cancel" }
                    p #status {}
                    progress #progress hidden {}
                    ul #active {}
                    ul #failures {}
                }
                hr;
                ul #listing {
                    @for entry in &entries {
                        li {
                            a href={ (utf8_percent_encode(&entry.name, SEGMENT)) @if entry.is_dir { "/" } } {
                                (entry.name)
                                @if entry.is_dir { "/" } @else if entry.is_symlink { "@" }
                            }
                        }
                    }
                }
                hr;
                div #drop { "Drop files or folders to upload them here" }
                script { (PreEscaped(SCRIPT)) }
            }
        }
    })
}
