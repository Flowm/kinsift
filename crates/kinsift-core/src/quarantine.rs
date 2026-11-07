//! kinsift's own trash for roots that are not on the home file system.
//!
//! Layout: `<quarantine dir>/<batch>/<path relative to the root>`, plus
//! `<batch>/manifest.json`, so a batch can be restored without the index.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::index::now_unix;

const MANIFEST: &str = "manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestFile {
    pub rel: String,
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub batch: String,
    pub created: i64,
    pub root: PathBuf,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone)]
pub struct Batch {
    pub id: String,
    pub dir: PathBuf,
    pub root: PathBuf,
    pub created: i64,
    pub files: usize,
    pub size: u64,
}

pub fn write_manifest(batch_dir: &Path, batch: &str, root: &Path, files: Vec<ManifestFile>) -> Result<()> {
    let path = batch_dir.join(MANIFEST);
    let mut manifest = read_manifest(batch_dir).unwrap_or(Manifest {
        batch: batch.to_owned(),
        created: now_unix(),
        root: root.to_path_buf(),
        files: Vec::new(),
    });
    manifest.files.extend(files);
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| Error::Other(e.to_string()))?;
    std::fs::write(&path, text).map_err(|e| Error::io(&path, e))
}

pub fn read_manifest(batch_dir: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(batch_dir.join(MANIFEST)).ok()?;
    serde_json::from_str(&text).ok()
}

/// All batches in the given quarantine directories, oldest first.
pub fn list(dirs: &[PathBuf]) -> Result<Vec<Batch>> {
    // Several roots may share one quarantine directory.
    let mut dirs: Vec<PathBuf> = dirs.iter().map(|d| d.canonicalize().unwrap_or_else(|_| d.clone())).collect();
    dirs.sort();
    dirs.dedup();
    let mut out = Vec::new();
    for dir in &dirs {
        let Ok(read) = std::fs::read_dir(dir) else { continue };
        for entry in read.filter_map(std::result::Result::ok) {
            let batch_dir = entry.path();
            let Some(m) = read_manifest(&batch_dir) else { continue };
            out.push(Batch {
                id: m.batch.clone(),
                dir: batch_dir,
                root: m.root.clone(),
                created: m.created,
                files: m.files.len(),
                size: m.files.iter().map(|f| f.size).sum(),
            });
        }
    }
    out.sort_by_key(|b| b.created);
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct RestoreOutcome {
    pub restored: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Move every file of a batch back to its original place.
pub fn restore(batch: &Batch) -> Result<RestoreOutcome> {
    let manifest = read_manifest(&batch.dir).ok_or_else(|| Error::Other("batch has no manifest".into()))?;
    let mut out = RestoreOutcome::default();
    for f in &manifest.files {
        let src = batch.dir.join(&f.rel);
        let dst = manifest.root.join(&f.rel);
        let result = if std::fs::symlink_metadata(&src).is_err() {
            Err("missing from the quarantine".to_owned())
        } else {
            crate::actions::move_no_copy(&src, &dst)
        };
        match result {
            Ok(()) => out.restored.push(f.rel.clone()),
            Err(reason) => out.failed.push((f.rel.clone(), reason)),
        }
    }
    forget_files(&batch.dir, &out.restored)?;
    Ok(out)
}

/// Permanently delete a batch. Returns the bytes freed (per manifest).
pub fn purge(batch: &Batch) -> Result<u64> {
    if read_manifest(&batch.dir).is_none() {
        return Err(Error::Other(format!("{} has no manifest; refusing to delete it", batch.dir.display())));
    }
    std::fs::remove_dir_all(&batch.dir).map_err(|e| Error::io(&batch.dir, e))?;
    Ok(batch.size)
}

/// Drop files (restored, or never moved) from a batch manifest; remove
/// the batch if it is empty.
pub fn forget_files(batch_dir: &Path, rels: &[String]) -> Result<()> {
    let Some(mut manifest) = read_manifest(batch_dir) else { return Ok(()) };
    manifest.files.retain(|f| !rels.contains(&f.rel));
    // Remove now-empty directories inside the batch.
    for rel in rels {
        let mut dir = batch_dir.join(rel);
        while dir.pop() && dir.starts_with(batch_dir) && dir != batch_dir {
            if std::fs::remove_dir(&dir).is_err() {
                break;
            }
        }
    }
    if manifest.files.is_empty() {
        let _ = std::fs::remove_file(batch_dir.join(MANIFEST));
        let _ = std::fs::remove_dir(batch_dir);
        return Ok(());
    }
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| Error::Other(e.to_string()))?;
    std::fs::write(batch_dir.join(MANIFEST), text).map_err(|e| Error::io(batch_dir, e))
}

/// Total size and age (seconds) of the oldest batch.
pub fn usage(batches: &[Batch]) -> (u64, Option<i64>) {
    let size = batches.iter().map(|b| b.size).sum();
    let oldest = batches.iter().map(|b| now_unix() - b.created).max();
    (size, oldest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_quarantine_dirs_are_listed_once() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join("q");
        std::fs::create_dir_all(q.join("b1")).unwrap();
        write_manifest(&q.join("b1"), "b1", dir.path(), vec![ManifestFile { rel: "x".into(), size: 1, mtime_ns: 0 }])
            .unwrap();
        let alias = dir.path().join("q/../q");
        assert_eq!(list(&[q.clone(), q, alias]).unwrap().len(), 1);
    }
}
