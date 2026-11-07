//! Walking roots, detecting changes, and running providers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::index::{ExtractionWrite, FileId, Index, now_unix, rel_string};
use crate::provider::{FileCtx, Priority, Provider, Stage};

/// Directory names never descended into.
const SKIP_DIRS: &[&str] =
    &["@eaDir", "#recycle", "#snapshot", "$RECYCLE.BIN", "System Volume Information", "lost+found"];

/// Above this share of vanished files, a rescan refuses to prune.
const PRUNE_GUARD_RATIO: f64 = 0.2;
/// Roots smaller than this are exempt from the ratio guard.
const PRUNE_GUARD_MIN_FILES: usize = 10;
/// Files processed per write transaction.
const CHUNK: usize = 512;

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Prune vanished files even if the mount guard would refuse.
    pub force_prune: bool,
}

#[derive(Debug, Clone)]
pub enum ScanEvent {
    Walking { root: PathBuf, found: usize },
    Extracting { root: PathBuf, done: usize, total: usize },
    Done(ScanReport),
}

#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub root: PathBuf,
    pub found: usize,
    pub new: usize,
    pub changed: usize,
    pub extracted: usize,
    pub errors: usize,
    pub walk_errors: usize,
    /// Files skipped because their path is not valid UTF-8 (the index
    /// stores text paths, so they could not be acted on correctly).
    pub non_utf8: usize,
    pub pruned: usize,
    /// Why pruning was skipped, if it was.
    pub prune_skipped: Option<String>,
}

struct Walked {
    rel: String,
    size: u64,
    mtime_ns: i64,
}

/// Scan one root: walk it, update the index, run providers on new or changed files.
pub fn scan_root(
    index: &mut Index,
    providers: &[Box<dyn Provider>],
    priority: &Priority,
    root: &Path,
    opts: &ScanOptions,
    progress: &(dyn Fn(ScanEvent) + Sync),
) -> Result<ScanReport> {
    let root = root.canonicalize().map_err(|e| Error::Other(format!("{}: {e} (is it mounted?)", root.display())))?;
    if !root.is_dir() {
        return Err(Error::Other(format!("{} is not a directory", root.display())));
    }
    if root.to_str().is_none() {
        return Err(Error::Other(format!("{}: the path is not valid UTF-8", root.display())));
    }
    // Overlapping roots would index the same file twice, and each copy
    // would count as the other's surviving duplicate.
    if let Some(other) = index.roots()?.into_iter().find(|r| r.path != root && overlaps(&r.path, &root)) {
        return Err(Error::Other(format!(
            "{} overlaps the known root {}; the same files would be indexed twice. \
             Scan only one of them (`kinsift roots remove` forgets a root).",
            root.display(),
            other.path.display()
        )));
    }
    let device = device_of(&root);
    let mut report = ScanReport { root: root.clone(), ..Default::default() };

    let walked = walk(&root, &mut report, progress);
    report.found = walked.len();

    let previous = index.root_by_path(&root)?;
    let stored = match &previous {
        Some(row) => index.stored_files(row.id)?,
        None => Default::default(),
    };
    let seen: HashSet<&str> = walked.iter().map(|w| w.rel.as_str()).collect();
    let missing: Vec<FileId> =
        stored.iter().filter(|(rel, _)| !seen.contains(rel.as_str())).map(|(_, f)| f.id).collect();
    for w in &walked {
        match stored.get(&w.rel) {
            None => report.new += 1,
            Some(s) if s.size != w.size || s.mtime_ns != w.mtime_ns => report.changed += 1,
            Some(_) => {}
        }
    }

    if !missing.is_empty() && !opts.force_prune {
        report.prune_skipped = if report.walk_errors > 0 {
            // Files below an unreadable folder look vanished but are not.
            Some(format!("{} entries could not be read, so vanished files were kept", report.walk_errors))
        } else {
            prune_guard(previous.as_ref().and_then(|r| r.device), device, walked.len(), stored.len(), missing.len())
        };
    }

    let root_row = index.ensure_root(&root)?;
    let mut ids = Vec::with_capacity(walked.len());
    for chunk in walked.chunks(2000) {
        let rows: Vec<(String, u64, i64)> = chunk.iter().map(|w| (w.rel.clone(), w.size, w.mtime_ns)).collect();
        ids.extend(index.upsert_files(root_row.id, &rows)?);
    }

    // Work out which providers still need to run on which file.
    let done = index.stored_extractions(root_row.id)?;
    let scan_providers: Vec<&dyn Provider> =
        providers.iter().map(AsRef::as_ref).filter(|p| p.stage() != Stage::Hash).collect();
    let mut work: Vec<(usize, Vec<usize>)> = Vec::new();
    for (i, w) in walked.iter().enumerate() {
        let abs = root.join(&w.rel);
        let ctx = FileCtx { root: &root, abs: &abs, rel: &w.rel, size: w.size, mtime_ns: w.mtime_ns };
        let have = done.get(&ids[i]);
        let todo: Vec<usize> = scan_providers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.applies(&ctx) && have.and_then(|h| h.get(p.name())) != Some(&p.version()))
            .map(|(pi, _)| pi)
            .collect();
        if !todo.is_empty() {
            work.push((i, todo));
        }
    }

    let total = work.len();
    let counter = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);
    for chunk in work.chunks(CHUNK) {
        let writes: Vec<ExtractionWrite> = chunk
            .par_iter()
            .flat_map_iter(|(i, todo)| {
                let w = &walked[*i];
                let abs = root.join(&w.rel);
                let ctx = FileCtx { root: &root, abs: &abs, rel: &w.rel, size: w.size, mtime_ns: w.mtime_ns };
                let writes: Vec<ExtractionWrite> = todo
                    .iter()
                    .map(|&pi| {
                        let p = scan_providers[pi];
                        let outcome = extract_guarded(p, &ctx);
                        if outcome.is_err() {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                        ExtractionWrite { file_id: ids[*i], provider: p.name(), version: p.version(), outcome }
                    })
                    .collect();
                let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_multiple_of(64) || n == total {
                    progress(ScanEvent::Extracting { root: root.clone(), done: n, total });
                }
                writes
            })
            .collect();
        index.write_extractions(writes, priority)?;
    }
    report.extracted = total;
    report.errors = errors.into_inner();

    if report.prune_skipped.is_none() && !missing.is_empty() {
        index.delete_files(&missing)?;
        report.pruned = missing.len();
    }
    let kept = if report.prune_skipped.is_some() { walked.len() + missing.len() } else { walked.len() };
    index.set_root_stats(root_row.id, device, kept as i64, now_unix())?;
    progress(ScanEvent::Done(report.clone()));
    Ok(report)
}

/// One path contains the other.
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Run a provider, turning a panic (a parser bug on a malformed file) into
/// an extraction error for that file instead of aborting the whole scan.
fn extract_guarded(
    p: &dyn Provider,
    ctx: &FileCtx,
) -> std::result::Result<Option<crate::provider::Extraction>, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.extract(ctx))).unwrap_or_else(|panic| {
        let msg = panic
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        Err(format!("{} provider crashed: {msg}", p.name()))
    })
}

/// Decide whether pruning vanished files looks unsafe. Returns the reason.
pub fn prune_guard(
    old_device: Option<u64>,
    device: Option<u64>,
    found: usize,
    stored: usize,
    missing: usize,
) -> Option<String> {
    if let (Some(old), Some(new)) = (old_device, device)
        && old != new
    {
        return Some(format!(
            "the root is on a different device than at the last scan ({old} → {new}); is the right volume mounted?"
        ));
    }
    if found == 0 {
        return Some("the root is empty; is it mounted?".into());
    }
    if stored >= PRUNE_GUARD_MIN_FILES && missing as f64 > stored as f64 * PRUNE_GUARD_RATIO {
        return Some(format!(
            "{missing} of {stored} files ({:.0}%) disappeared at once",
            missing as f64 * 100.0 / stored as f64
        ));
    }
    None
}

fn walk(root: &Path, report: &mut ScanReport, progress: &(dyn Fn(ScanEvent) + Sync)) -> Vec<Walked> {
    let mut out = Vec::new();
    let iter =
        walkdir::WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| e.depth() == 0 || !is_skipped(e));
    for entry in iter {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                log::warn!("walk: {err}");
                report.walk_errors += 1;
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                log::warn!("stat: {err}");
                report.walk_errors += 1;
                continue;
            }
        };
        if entry.path().to_str().is_none() {
            log::warn!("skipping non-UTF-8 path: {}", entry.path().display());
            report.non_utf8 += 1;
            continue;
        }
        let rel = rel_string(entry.path().strip_prefix(root).unwrap_or(entry.path()));
        out.push(Walked { rel, size: meta.len(), mtime_ns: mtime_ns(&meta) });
        if out.len().is_multiple_of(1000) {
            progress(ScanEvent::Walking { root: root.to_path_buf(), found: out.len() });
        }
    }
    progress(ScanEvent::Walking { root: root.to_path_buf(), found: out.len() });
    out
}

fn is_skipped(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    name.starts_with('.') || (entry.file_type().is_dir() && SKIP_DIRS.contains(&name.as_ref()))
}

pub fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Device id of the filesystem holding `path` (Unix only).
pub fn device_of(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| m.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_device_change() {
        assert!(prune_guard(Some(1), Some(2), 100, 100, 1).unwrap().contains("different device"));
    }

    #[test]
    fn guard_empty_root() {
        assert!(prune_guard(None, None, 0, 5, 5).unwrap().contains("empty"));
    }

    #[test]
    fn guard_ratio() {
        assert!(prune_guard(Some(1), Some(1), 70, 100, 30).unwrap().contains("30 of 100"));
        assert_eq!(prune_guard(Some(1), Some(1), 90, 100, 10), None);
        // Small roots are exempt from the ratio rule.
        assert_eq!(prune_guard(Some(1), Some(1), 1, 4, 3), None);
    }

    struct Panicky;
    impl Provider for Panicky {
        fn name(&self) -> &'static str {
            "panicky"
        }
        fn version(&self) -> u32 {
            1
        }
        fn stage(&self) -> Stage {
            Stage::Name
        }
        fn applies(&self, _: &FileCtx) -> bool {
            true
        }
        fn extract(&self, ctx: &FileCtx) -> std::result::Result<Option<crate::provider::Extraction>, String> {
            if ctx.rel.contains("bad") { panic!("parser bug") } else { Ok(None) }
        }
    }

    fn scan(index: &mut Index, root: &Path) -> ScanReport {
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Panicky)];
        scan_root(index, &providers, &Priority::default(), root, &ScanOptions::default(), &|_| {}).unwrap()
    }

    #[test]
    fn a_panicking_provider_fails_one_file_not_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["good.mkv", "bad.mkv"] {
            std::fs::write(dir.path().join(f), b"x").unwrap();
        }
        let mut index = Index::open_in_memory().unwrap();
        let r = scan(&mut index, dir.path());
        assert_eq!((r.found, r.errors), (2, 1));
    }

    #[test]
    fn overlapping_roots_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(inner.join("a.mkv"), b"x").unwrap();
        let mut index = Index::open_in_memory().unwrap();
        scan(&mut index, &inner);
        let run = |index: &mut Index, root: &Path| {
            let providers: Vec<Box<dyn Provider>> = Vec::new();
            scan_root(index, &providers, &Priority::default(), root, &ScanOptions::default(), &|_| {})
        };
        assert!(run(&mut index, dir.path()).unwrap_err().to_string().contains("overlaps"));
        assert!(run(&mut index, &inner).is_ok(), "rescanning the same root is fine");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_skipped() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.mkv"), b"x").unwrap();
        std::fs::write(dir.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9.mkv")), b"x").unwrap();
        let mut index = Index::open_in_memory().unwrap();
        let r = scan(&mut index, dir.path());
        assert_eq!((r.found, r.non_utf8), (1, 1));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_folders_do_not_prune_their_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        for i in 0..20 {
            std::fs::write(dir.path().join(format!("{i}.mkv")), b"x").unwrap();
        }
        std::fs::write(sub.join("inside.mkv"), b"x").unwrap();
        let mut index = Index::open_in_memory().unwrap();
        assert_eq!(scan(&mut index, dir.path()).found, 21);
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&sub).is_ok(); // running as root ignores permissions
        let r = scan(&mut index, dir.path());
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !readable {
            assert!(r.walk_errors > 0);
            assert_eq!(r.pruned, 0);
            assert!(r.prune_skipped.unwrap().contains("could not be read"));
        }
    }
}
