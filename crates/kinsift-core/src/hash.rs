//! On-demand content hashing in stages, so only plausible duplicates are read:
//! 1. group by size (files with a unique size cannot be duplicates),
//! 2. blake3 over size + first and last 64 KiB ("partial hash"),
//! 3. full blake3 only where partial hashes collide.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::domain::{ContentMeta, Domain};
use crate::error::Result;
use crate::index::{ExtractionWrite, FileEntry, FileId, Index};
use crate::provider::{Extraction, Priority};

pub const PROVIDER: &str = "hash";
pub const VERSION: u32 = 1;
const EDGE: u64 = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub struct HashReport {
    /// Files sharing their size with at least one other file.
    pub candidates: usize,
    pub partial_hashed: usize,
    pub full_hashed: usize,
    pub errors: usize,
    pub bytes_read: u64,
    /// Groups of byte-identical files found.
    pub duplicate_groups: usize,
}

pub fn partial_hash(path: &Path, size: u64) -> std::io::Result<(String, u64)> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    h.update(&size.to_le_bytes());
    let mut buf = vec![0u8; EDGE as usize];
    let n = read_up_to(&mut f, &mut buf)?;
    h.update(&buf[..n]);
    let mut read = n as u64;
    if size > 2 * EDGE {
        f.seek(SeekFrom::Start(size - EDGE))?;
        let n = read_up_to(&mut f, &mut buf)?;
        h.update(&buf[..n]);
        read += n as u64;
    } else if size > EDGE {
        let n = read_up_to(&mut f, &mut buf)?;
        h.update(&buf[..n]);
        read += n as u64;
    }
    Ok((h.finalize().to_hex().to_string(), read))
}

pub fn full_hash(path: &Path) -> std::io::Result<(String, u64)> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((h.finalize().to_hex().to_string(), total))
}

fn read_up_to(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Hash the plausible duplicates among `entries` and store the results.
/// Hashes already in the index (for unchanged files) are reused.
pub fn hash_duplicates(
    index: &mut Index,
    entries: &[FileEntry],
    priority: &Priority,
    progress: &(dyn Fn(&str, usize, usize) + Sync),
) -> Result<HashReport> {
    let mut report = HashReport::default();
    let mut by_size: HashMap<u64, Vec<&FileEntry>> = HashMap::new();
    for e in entries.iter().filter(|e| e.size > 0) {
        by_size.entry(e.size).or_default().push(e);
    }
    let candidates: Vec<&FileEntry> = by_size.into_values().filter(|g| g.len() > 1).flatten().collect();
    report.candidates = candidates.len();

    let bytes = AtomicU64::new(0);
    let errors = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let stored = |e: &FileEntry, key: &str| e.get(key).and_then(|v| v.as_str()).map(str::to_owned);

    // Stage 2: partial hashes.
    let todo = candidates.iter().filter(|e| stored(e, "content.partial_hash").is_none()).count();
    let partial: Vec<(FileId, Option<String>)> = candidates
        .par_iter()
        .map(|e| {
            if let Some(h) = stored(e, "content.partial_hash") {
                return (e.id, Some(h));
            }
            let r = partial_hash(&e.abs(), e.size);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(16) || n == todo {
                progress("partial hash", n, todo);
            }
            match r {
                Ok((h, read)) => {
                    bytes.fetch_add(read, Ordering::Relaxed);
                    (e.id, Some(h))
                }
                Err(err) => {
                    log::warn!("hash {}: {err}", e.abs().display());
                    errors.fetch_add(1, Ordering::Relaxed);
                    (e.id, None)
                }
            }
        })
        .collect();
    report.partial_hashed = todo;
    let partial: HashMap<FileId, String> = partial.into_iter().filter_map(|(id, h)| Some((id, h?))).collect();

    // Stage 3: full hashes where (size, partial) collide.
    let mut by_partial: HashMap<(u64, &str), Vec<&FileEntry>> = HashMap::new();
    for e in &candidates {
        if let Some(p) = partial.get(&e.id) {
            by_partial.entry((e.size, p.as_str())).or_default().push(e);
        }
    }
    let colliding: Vec<&FileEntry> = by_partial.into_values().filter(|g| g.len() > 1).flatten().collect();
    let todo = colliding.iter().filter(|e| stored(e, "content.hash").is_none()).count();
    done.store(0, Ordering::Relaxed);
    let full: HashMap<FileId, String> = colliding
        .par_iter()
        .filter_map(|e| {
            if let Some(h) = stored(e, "content.hash") {
                return Some((e.id, h));
            }
            let r = full_hash(&e.abs());
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress("full hash", n, todo);
            match r {
                Ok((h, read)) => {
                    bytes.fetch_add(read, Ordering::Relaxed);
                    Some((e.id, h))
                }
                Err(err) => {
                    log::warn!("hash {}: {err}", e.abs().display());
                    errors.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        })
        .collect();
    report.full_hashed = todo;

    let mut groups: HashMap<&str, usize> = HashMap::new();
    for h in full.values() {
        *groups.entry(h.as_str()).or_default() += 1;
    }
    report.duplicate_groups = groups.values().filter(|&&n| n > 1).count();

    let writes: Vec<ExtractionWrite> = candidates
        .iter()
        .filter_map(|e| {
            let p = partial.get(&e.id)?.clone();
            let meta = ContentMeta { partial_hash: Some(p), hash: full.get(&e.id).cloned() };
            Some(ExtractionWrite {
                file_id: e.id,
                provider: PROVIDER,
                version: VERSION,
                outcome: Ok(Some(Extraction { sets: vec![meta.into_fields()], raw: None })),
            })
        })
        .collect();
    for chunk in writes.chunks(512) {
        index.write_extractions(chunk.to_vec(), priority)?;
    }
    report.errors = errors.into_inner();
    report.bytes_read = bytes.into_inner();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Filter;
    use crate::scan::mtime_ns;

    #[test]
    fn stages_find_identical_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let big = |fill: u8, tail: u8| {
            let mut v = vec![fill; 300_000];
            *v.last_mut().unwrap() = tail;
            v[150_000] = tail; // differs only in the middle for the "near" file
            v
        };
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("a.bin", big(1, 9)),
            ("b.bin", big(1, 9)), // identical to a
            ("c.bin", {
                let mut v = big(1, 9);
                v[150_001] = 7; // same size, same head/tail, different middle
                v
            }),
            ("d.bin", vec![5; 10]), // unique size
        ];
        let mut index = Index::open_in_memory().unwrap();
        let row = index.ensure_root(&root).unwrap();
        let mut rows = Vec::new();
        for (name, data) in &files {
            std::fs::write(root.join(name), data).unwrap();
            let m = std::fs::metadata(root.join(name)).unwrap();
            rows.push((name.to_string(), m.len(), mtime_ns(&m)));
        }
        index.upsert_files(row.id, &rows).unwrap();
        let entries = index.load_entries(None, None).unwrap();
        let priority = Priority::default();
        let r = hash_duplicates(&mut index, &entries, &priority, &|_, _, _| {}).unwrap();
        assert_eq!(r.candidates, 3);
        assert_eq!(r.full_hashed, 3, "a, b and c share head and tail");
        assert_eq!(r.duplicate_groups, 1);

        let hashed = index.load_entries(Some(&Filter::parse("content.hash is not null").unwrap()), None).unwrap();
        assert_eq!(hashed.len(), 3);
        // A second run reuses stored hashes.
        let entries = index.load_entries(None, None).unwrap();
        let r = hash_duplicates(&mut index, &entries, &priority, &|_, _, _| {}).unwrap();
        assert_eq!((r.partial_hashed, r.full_hashed, r.bytes_read), (0, 0, 0));
    }
}
