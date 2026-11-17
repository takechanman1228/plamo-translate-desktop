//! Local copies of finished file translations. Publish the metadata after the document.
use chrono::{DateTime, Local, Utc};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
struct Metadata {
    filename: String,
    target: String,
    extension: String,
    created: i64,
}

pub(crate) struct Entry {
    metadata: Metadata,
    pub output: PathBuf,
}

impl Entry {
    pub fn filename(&self) -> &str {
        &self.metadata.filename
    }

    pub fn target(&self) -> &str {
        &self.metadata.target
    }

    pub fn completed_at(&self) -> String {
        DateTime::from_timestamp_micros(self.metadata.created)
            .map(|date| {
                date.with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default()
    }

    fn from_dir(dir: &Path, metadata: Metadata) -> io::Result<Self> {
        if !matches!(metadata.extension.as_str(), "txt" | "md" | "docx" | "pptx") {
            return Err(io::Error::other("Unsupported history file type"));
        }
        let stem = Path::new(&metadata.filename)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let output = dir.join(format!("{stem}.translated.{}", metadata.extension));
        Ok(Self { metadata, output })
    }
}

pub(crate) fn load(root: &Path) -> io::Result<Vec<Entry>> {
    let dirs = match fs::read_dir(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => result?,
    };
    let mut entries = Vec::new();
    for dir in dirs {
        let dir = dir?.path();
        let Ok(bytes) = fs::read(dir.join("entry.json")) else {
            continue;
        };
        if let Ok(metadata) = serde_json::from_slice(&bytes)
            && let Ok(entry) = Entry::from_dir(&dir, metadata)
            && entry.output.is_file()
        {
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.metadata.created));
    Ok(entries)
}

pub(crate) fn record(
    root: &Path,
    source: &Path,
    target: String,
    extension: &str,
    bytes: &[u8],
) -> io::Result<Entry> {
    fs::create_dir_all(root)?;
    let dir = tempfile::Builder::new()
        .prefix("translation-")
        .tempdir_in(root)?;
    let entry = Entry::from_dir(
        dir.path(),
        Metadata {
            filename: source
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            target,
            extension: extension.to_owned(),
            created: Utc::now().timestamp_micros(),
        },
    )?;
    files::save_new(source, &entry.output, bytes).map_err(io::Error::other)?;
    files::save_new(
        source,
        &dir.path().join("entry.json"),
        &serde_json::to_vec(&entry.metadata)?,
    )
    .map_err(io::Error::other)?;
    let _ = dir.keep();
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_named_results_survive_reloading_without_the_original() {
        let root = tempfile::tempdir().unwrap();
        let source = Path::new("example.txt");
        let first = record(root.path(), source, "ja".into(), "txt", b"first").unwrap();
        let second = record(root.path(), source, "en".into(), "txt", b"second").unwrap();
        let entries = load(root.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].output, second.output);
        assert_eq!(fs::read(&entries[1].output).unwrap(), b"first");
        assert_eq!(entries[1].output, first.output);
        assert_eq!(entries[1].filename(), "example.txt");
    }
}
