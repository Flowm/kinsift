//! Changing files on disk, safely: delete plans, strategies, rename, undo.
//!
//! Safety rules (see docs/DECISIONS.md, Q50–Q56):
//! - each root has one delete strategy, chosen up front; there is never a
//!   silent fallback (no copying to a trash on another file system),
//! - every file is re-checked right before it is touched,
//! - a plan may never remove the last existing member of a group,
//! - every change is journaled so it can be undone.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::{Config, RootConfig};
use crate::error::{Error, Result};
use crate::index::{FileEntry, FileId, Index, RootRow, now_unix, path_str, rel_string};
use crate::quarantine::{self, ManifestFile};
use crate::scan::{device_of, mtime_ns};

pub const QUARANTINE_DIR: &str = ".kinsift-trash";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyKind {
    Trash,
    Quarantine,
    Permanent,
}

impl StrategyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StrategyKind::Trash => "trash",
            StrategyKind::Quarantine => "quarantine",
            StrategyKind::Permanent => "permanent",
        }
    }
}

/// How (and whether) files in one root may be changed.
#[derive(Debug, Clone)]
pub struct RootPolicy {
    pub root: PathBuf,
    pub kind: StrategyKind,
    pub quarantine_dir: PathBuf,
    /// Device of the root directory; files on another device are refused.
    pub device: Option<u64>,
    /// Why no changes are possible on this root right now.
    pub blocked: Option<String>,
    pub explanation: String,
}

impl RootPolicy {
    /// Where this root's files of `batch` go in quarantine. A configured
    /// quarantine directory may be shared by several roots, so the batch
    /// folder there carries a tag of the root: each root gets its own
    /// manifest and relative paths of different roots cannot collide.
    pub fn batch_dir(&self, batch: &str) -> PathBuf {
        if self.quarantine_dir == self.root.join(QUARANTINE_DIR) {
            return self.quarantine_dir.join(batch);
        }
        let tag = blake3::hash(path_str(&self.root).as_bytes()).to_hex();
        self.quarantine_dir.join(format!("{batch}-{}", &tag[..8]))
    }
}

/// Device of the file system that holds the user's trash.
fn home_trash_device() -> Option<u64> {
    dirs::data_dir().filter(|d| d.exists()).or_else(dirs::home_dir).and_then(|d| device_of(&d))
}

/// Decide the delete strategy for a root.
pub fn root_policy(row: &RootRow, cfg: Option<&RootConfig>) -> RootPolicy {
    let root = row.path.clone();
    let quarantine_dir = cfg.and_then(|c| c.quarantine.clone()).unwrap_or_else(|| root.join(QUARANTINE_DIR));
    let device = device_of(&root);
    let same_fs_as_home = matches!((device, home_trash_device()), (Some(a), Some(b)) if a == b);
    let mut blocked = None;
    let requested = cfg.and_then(|c| c.delete.as_deref()).unwrap_or("auto");
    let kind = match requested {
        "auto" if same_fs_as_home => StrategyKind::Trash,
        "auto" | "quarantine" => StrategyKind::Quarantine,
        "trash" => {
            if !same_fs_as_home {
                blocked = Some(
                    "`delete = \"trash\"` is configured, but this root is not on your home file system; \
                     the trash would copy files across file systems. Use `quarantine` instead."
                        .to_owned(),
                );
            }
            StrategyKind::Trash
        }
        "permanent" => StrategyKind::Permanent,
        other => {
            blocked = Some(format!("unknown delete strategy `{other}` in the config"));
            StrategyKind::Quarantine
        }
    };
    if !root.is_dir() {
        blocked = Some("the root is not accessible; is it mounted?".into());
    } else if let (Some(old), Some(now)) = (row.device, device)
        && old != now
    {
        blocked = Some("the root is on a different device than at the last scan; rescan first".into());
    } else if is_read_only(&root) {
        blocked = Some("the root is read-only".into());
    }
    let explanation = match kind {
        StrategyKind::Trash => "move to the OS trash".to_owned(),
        StrategyKind::Quarantine => format!("move to quarantine in {}", quarantine_dir.display()),
        StrategyKind::Permanent => "delete PERMANENTLY (configured)".to_owned(),
    };
    RootPolicy { root, kind, quarantine_dir, device, blocked, explanation }
}

/// Policies for every known root.
pub fn root_policies(index: &Index, config: &Config) -> Result<Vec<RootPolicy>> {
    Ok(index.roots()?.iter().map(|r| root_policy(r, config.root_config(&r.path))).collect())
}

fn is_read_only(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use rustix::fs::{Access, StatVfsMountFlags, access, statvfs};
        let ro_mount = statvfs(path).map(|s| s.f_flag.contains(StatVfsMountFlags::RDONLY)).unwrap_or(false);
        ro_mount || access(path, Access::WRITE_OK).is_err()
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path).map(|m| m.permissions().readonly()).unwrap_or(true)
    }
}

/// `revalidate`, plus: the file must be on its root's file system. A disk
/// mounted inside a root is walked, but deleting from it could copy across
/// file systems (trash) or fail halfway (quarantine).
pub fn check_file(entry: &FileEntry, root_device: Option<u64>) -> std::result::Result<(), String> {
    revalidate(entry)?;
    if let (Some(root), Some(file)) = (root_device, device_of(&entry.abs()))
        && root != file
    {
        return Err("is on another file system than its root (a mount inside the root); add it as its own root".into());
    }
    Ok(())
}

/// Is the file still exactly what the index says?
pub fn revalidate(entry: &FileEntry) -> std::result::Result<(), String> {
    revalidated(entry).map(|_| ())
}

fn revalidated(entry: &FileEntry) -> std::result::Result<std::fs::Metadata, String> {
    let abs = entry.abs();
    let meta = match std::fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Err("no longer exists".into()),
        Err(e) => return Err(e.to_string()),
    };
    if !meta.is_file() {
        return Err("is not a regular file".into());
    }
    if meta.len() != entry.size || mtime_ns(&meta) != entry.mtime_ns {
        return Err("changed since the last scan; rescan first".into());
    }
    Ok(meta)
}

/// What a path refers to on disk. Two index entries can be the same file:
/// overlapping roots, bind mounts or hard links. Removing a hard link frees
/// nothing, so treating it as the same file only ever refuses more.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FileKey {
    #[cfg(unix)]
    Inode(u64, u64),
    #[cfg(not(unix))]
    Path(PathBuf),
}

#[allow(unused_variables)]
fn file_key(entry: &FileEntry, meta: &std::fs::Metadata) -> FileKey {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileKey::Inode(meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        FileKey::Path(entry.abs().canonicalize().unwrap_or_else(|_| entry.abs()))
    }
}

#[derive(Debug, Clone)]
pub struct PlanItem {
    pub entry: FileEntry,
    /// The duplicate group this file belongs to, if any.
    pub group: Option<String>,
    /// Not part of any duplicate group: removing it may remove the only copy.
    pub only_copy: bool,
    pub include: bool,
    /// Why this item cannot be executed.
    pub refused: Option<String>,
}

impl PlanItem {
    pub fn will_run(&self) -> bool {
        self.include && self.refused.is_none()
    }
}

/// A reviewed set of files to remove.
#[derive(Debug, Clone)]
pub struct Plan {
    pub items: Vec<PlanItem>,
    pub policies: Vec<RootPolicy>,
    /// Members of every group touched by the plan.
    groups: HashMap<String, Vec<FileEntry>>,
    /// Group members that still exist unchanged, and what they are on
    /// disk (see `refresh`).
    alive: HashMap<FileId, FileKey>,
    /// Selected files that cannot be touched, and why (see `refresh`).
    problems: HashMap<FileId, String>,
}

impl Plan {
    /// Build a plan. `groups` maps group ids to all members (from the view);
    /// files not in any group are flagged as "only copy" and excluded.
    pub fn new(
        index: &Index,
        config: &Config,
        selected: Vec<FileEntry>,
        groups: HashMap<String, Vec<FileEntry>>,
    ) -> Result<Plan> {
        let group_of: HashMap<FileId, String> =
            groups.iter().flat_map(|(g, members)| members.iter().map(move |m| (m.id, g.clone()))).collect();
        let roots: HashSet<&Path> = selected.iter().map(|e| e.root.as_path()).collect();
        let policies = index
            .roots()?
            .iter()
            .filter(|r| roots.contains(r.path.as_path()))
            .map(|r| root_policy(r, config.root_config(&r.path)))
            .collect();
        let items = selected
            .into_iter()
            .map(|entry| {
                let group = group_of.get(&entry.id).cloned();
                let only_copy = group.is_none();
                PlanItem { entry, group, only_copy, include: !only_copy, refused: None }
            })
            .collect();
        let mut plan = Plan { items, policies, groups, alive: HashMap::new(), problems: HashMap::new() };
        plan.refresh();
        Ok(plan)
    }

    /// Re-read the disk: which group members still exist unchanged, and
    /// which selected files changed or live on another file system than
    /// their root. Then re-check. This stats files, so the review screen
    /// calls it once, not on every toggle; `execute` calls it again.
    pub fn refresh(&mut self) {
        self.alive = self
            .groups
            .values()
            .flatten()
            .filter_map(|m| revalidated(m).ok().map(|meta| (m.id, file_key(m, &meta))))
            .collect();
        self.problems.clear();
        for item in &self.items {
            let policy = self.policies.iter().find(|p| p.root == *item.entry.root);
            if let Some(reason) = check_file(&item.entry, policy.and_then(|p| p.device)).err() {
                self.problems.insert(item.entry.id, reason);
            }
        }
        self.check();
    }

    pub fn policy(&self, root: &Path) -> Option<&RootPolicy> {
        self.policies.iter().find(|p| p.root == root)
    }

    /// Re-evaluate which items can run from the last `refresh`. Cheap.
    pub fn check(&mut self) {
        for item in &mut self.items {
            item.refused = match self.policies.iter().find(|p| p.root == *item.entry.root) {
                None => Some("root is not in the index".into()),
                Some(p) => p.blocked.clone().or_else(|| self.problems.get(&item.entry.id).cloned()),
            };
        }
        // Never remove the last existing member of a group. A member that
        // is the same file on disk as one being removed does not count.
        let removing: HashSet<FileId> = self.items.iter().filter(|i| i.will_run()).map(|i| i.entry.id).collect();
        let removing_keys: HashSet<&FileKey> = removing.iter().filter_map(|id| self.alive.get(id)).collect();
        let mut doomed_groups = HashSet::new();
        for (gid, members) in &self.groups {
            let touched = members.iter().any(|m| removing.contains(&m.id));
            if !touched {
                continue;
            }
            let survivor_alive = members.iter().any(|m| {
                !removing.contains(&m.id) && self.alive.get(&m.id).is_some_and(|k| !removing_keys.contains(k))
            });
            if !survivor_alive {
                doomed_groups.insert(gid.clone());
            }
        }
        for item in &mut self.items {
            if item.will_run() && item.group.as_ref().is_some_and(|g| doomed_groups.contains(g)) {
                item.refused = Some("would remove the last remaining copy in its group".into());
            }
        }
    }

    pub fn toggle(&mut self, i: usize) {
        if let Some(item) = self.items.get_mut(i) {
            item.include = !item.include;
        }
        self.check();
    }

    /// Number of files and bytes that will be removed.
    pub fn totals(&self) -> (usize, u64) {
        self.items.iter().filter(|i| i.will_run()).fold((0, 0), |(n, b), i| (n + 1, b + i.entry.size))
    }

    /// Per root: (policy, files, bytes) of what will run.
    pub fn per_root(&self) -> Vec<(&RootPolicy, usize, u64)> {
        self.policies
            .iter()
            .map(|p| {
                let (n, b) = self
                    .items
                    .iter()
                    .filter(|i| i.will_run() && *i.entry.root == p.root)
                    .fold((0, 0), |(n, b), i| (n + 1, b + i.entry.size));
                (p, n, b)
            })
            .filter(|(_, n, _)| *n > 0)
            .collect()
    }

    /// Permanent deletion needs a typed confirmation.
    pub fn needs_typed_confirmation(&self) -> bool {
        self.items
            .iter()
            .any(|i| i.will_run() && self.policy(&i.entry.root).is_some_and(|p| p.kind == StrategyKind::Permanent))
    }
}

#[derive(Debug, Clone)]
pub struct Done {
    pub entry: FileEntry,
    pub op: StrategyKind,
    pub dst: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub batch: String,
    pub done: Vec<Done>,
    pub skipped: Vec<(PathBuf, String)>,
    /// Things that went wrong after a file was already handled.
    pub warnings: Vec<String>,
}

pub fn new_batch_id() -> String {
    let now = now_unix();
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let t = crate::value::format_unix(now).replace(['-', ':'], "").replace(' ', "-");
    format!("{t}{:02}-{:04x}", now % 60, nanos & 0xffff)
}

/// Execute a plan. Re-checks everything first and file by file.
///
/// Crash safety: quarantine manifests are written before the first move,
/// and each file's journal entry before the file is touched (and removed
/// again if that fails). A batch interrupted halfway stays listed,
/// restorable and undoable; entries for files that never moved simply
/// report "missing" or "occupied" on restore.
pub fn execute(index: &mut Index, plan: &mut Plan) -> Result<Outcome> {
    plan.refresh();
    let mut out = Outcome { batch: new_batch_id(), ..Default::default() };
    // Included, but no longer possible (changed or gone since the review).
    for item in plan.items.iter().filter(|i| i.include) {
        if let Some(reason) = &item.refused {
            out.skipped.push((item.entry.abs(), reason.clone()));
        }
    }
    let items: Vec<PlanItem> = plan.items.iter().filter(|i| i.will_run()).cloned().collect();

    let mut manifests: BTreeMap<PathBuf, (PathBuf, Vec<ManifestFile>)> = BTreeMap::new();
    for item in &items {
        if let Some(p) = plan.policy(&item.entry.root).filter(|p| p.kind == StrategyKind::Quarantine) {
            let e = &item.entry;
            manifests
                .entry(p.batch_dir(&out.batch))
                .or_insert_with(|| (p.root.clone(), Vec::new()))
                .1
                .push(ManifestFile { rel: e.rel.clone(), size: e.size, mtime_ns: e.mtime_ns });
        }
    }
    for (batch_dir, (root, files)) in &manifests {
        std::fs::create_dir_all(batch_dir).map_err(|e| Error::io(batch_dir, e))?;
        quarantine::write_manifest(batch_dir, &out.batch, root, files.clone())?;
    }
    let mut not_moved: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();

    for item in items {
        let entry = &item.entry;
        let abs = entry.abs();
        let Some(policy) = plan.policy(&entry.root).cloned() else { continue };
        let batch_dir = policy.batch_dir(&out.batch);
        let dst = (policy.kind == StrategyKind::Quarantine).then(|| batch_dir.join(&entry.rel));
        let mut skip = |reason: String, out: &mut Outcome| {
            if dst.is_some() {
                not_moved.entry(batch_dir.clone()).or_default().push(entry.rel.clone());
            }
            out.skipped.push((abs.clone(), reason));
        };
        if let Err(reason) = check_file(entry, policy.device) {
            skip(reason, &mut out);
            continue;
        }
        let kind = policy.kind.as_str();
        let action = match index.record_action(
            &out.batch,
            kind,
            kind,
            &policy.root,
            &abs,
            dst.as_deref(),
            entry.size,
            entry.mtime_ns,
        ) {
            Ok(id) => id,
            Err(e) => {
                skip(format!("could not journal it, so it was left alone: {e}"), &mut out);
                continue;
            }
        };
        let result = match (policy.kind, &dst) {
            (StrategyKind::Trash, _) => trash::delete(&abs).map_err(|e| format!("trash: {e}")),
            (StrategyKind::Quarantine, Some(dst)) => move_no_copy(&abs, dst),
            (StrategyKind::Permanent, _) => std::fs::remove_file(&abs).map_err(|e| e.to_string()),
            (StrategyKind::Quarantine, None) => unreachable!("quarantine always has a destination"),
        };
        match result {
            Ok(()) => {
                if let Err(e) = index.delete_files(&[entry.id]) {
                    out.warnings.push(format!("{}: done, but the index still lists it ({e}); rescan", abs.display()));
                }
                out.done.push(Done { entry: entry.clone(), op: policy.kind, dst });
            }
            Err(reason) => {
                if let Err(e) = index.delete_action(action) {
                    out.warnings.push(format!("{}: journal entry left behind ({e})", abs.display()));
                }
                skip(reason, &mut out);
            }
        }
    }
    for (batch_dir, rels) in not_moved {
        quarantine::forget_files(&batch_dir, &rels)?;
    }
    Ok(out)
}

/// Move a file within its file system. Never copies, never replaces.
pub(crate) fn move_no_copy(src: &Path, dst: &Path) -> std::result::Result<(), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    rename_no_replace(src, dst).map_err(|e| match e.kind() {
        ErrorKind::AlreadyExists => format!("{} already exists", dst.display()),
        ErrorKind::CrossesDevices => format!("{} is on another file system; nothing was copied", dst.display()),
        _ => e.to_string(),
    })
}

/// `rename`, but fail if `dst` exists (also as a dangling symlink).
fn rename_no_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        use rustix::io::Errno;
        match renameat_with(CWD, src, CWD, dst, RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(()),
            // The file system cannot do this atomically; check, then rename.
            Err(e) if e == Errno::INVAL || e == Errno::NOSYS || e == Errno::OPNOTSUPP => {}
            Err(e) => return Err(e.into()),
        }
    }
    if std::fs::symlink_metadata(dst).is_ok() {
        return Err(ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(src, dst)
}

/// Rename a file inside its directory. Returns the new relative path.
pub fn rename(index: &mut Index, policy: &RootPolicy, entry: &FileEntry, new_name: &str) -> Result<String> {
    let new_name = new_name.trim();
    if new_name.is_empty() || new_name.contains(['/', '\\', '\0']) || new_name == "." || new_name == ".." {
        return Err(Error::Other("invalid file name".into()));
    }
    if let Some(reason) = &policy.blocked {
        return Err(Error::Other(reason.clone()));
    }
    revalidate(entry).map_err(Error::Other)?;
    let abs = entry.abs();
    let new_abs = abs.with_file_name(new_name);
    if new_abs == abs {
        return Err(Error::Other("the name is unchanged".into()));
    }
    // Journal first, so a rename that happened can always be undone.
    let action = index.record_action(
        &new_batch_id(),
        "rename",
        "rename",
        &policy.root,
        &abs,
        Some(&new_abs),
        entry.size,
        entry.mtime_ns,
    )?;
    if let Err(e) = move_no_copy(&abs, &new_abs) {
        // If this fails too, undo sees the file never moved and skips it.
        let _ = index.delete_action(action);
        return Err(Error::Other(e));
    }
    let new_rel = rel_string(new_abs.strip_prefix(&policy.root).unwrap_or(&new_abs));
    if index.rename_file(entry.id, &new_rel).is_err() {
        // The file did move. Drop the stale row; the caller re-reads `new_rel`.
        let _ = index.delete_by_rel(&policy.root, &entry.rel);
    }
    Ok(new_rel)
}

#[derive(Debug, Clone, Default)]
pub struct UndoOutcome {
    pub batch: String,
    /// Files back at their original place: (root, rel). Refresh these.
    pub restored: Vec<(PathBuf, String)>,
    /// Paths that no longer exist after the undo: (root, rel). Drop from the index.
    pub removed: Vec<(PathBuf, String)>,
    pub failed: Vec<(PathBuf, String)>,
}

/// What undoing one journaled action did.
enum Step {
    Restored,
    /// Journaled, but kinsift stopped before touching the file.
    NeverMoved,
    /// Could not be undone now; it stays undoable (e.g. the path is occupied).
    Failed(String),
    /// Can never be undone (e.g. the trash was emptied); it is retired so it
    /// no longer blocks undoing older batches.
    Gone(String),
}

/// Undo one journaled batch (trash, quarantine and rename actions).
pub fn undo_batch(index: &mut Index, batch: &str) -> Result<UndoOutcome> {
    let mut out = UndoOutcome { batch: batch.to_owned(), ..Default::default() };
    let mut actions = index.actions(Some(batch))?;
    actions.reverse();
    let mut quarantined: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for a in actions.into_iter().filter(|a| !a.undone) {
        let rel = |p: &Path| rel_string(p.strip_prefix(&a.root).unwrap_or(p));
        let exists = |p: &Path| std::fs::symlink_metadata(p).is_ok();
        let step = match a.op.as_str() {
            "quarantine" | "rename" => match &a.dst {
                Some(dst) if exists(&a.src) && !exists(dst) => Step::NeverMoved,
                Some(_) if exists(&a.src) => Step::Failed(format!("{} is occupied", a.src.display())),
                Some(dst) if !exists(dst) => Step::Gone(format!("{} no longer exists", dst.display())),
                Some(dst) => match move_no_copy(dst, &a.src) {
                    Ok(()) => {
                        if a.op == "rename" {
                            out.removed.push((a.root.clone(), rel(dst)));
                        } else if let Some(batch_dir) = dst.ancestors().find(|p| is_batch_dir(p, batch)) {
                            quarantined.entry(batch_dir.to_path_buf()).or_default().push(rel(&a.src));
                        }
                        Step::Restored
                    }
                    Err(reason) => Step::Failed(reason),
                },
                None => Step::Gone("journal entry has no destination".into()),
            },
            "trash" => restore_from_trash(&a.src),
            other => Step::Gone(format!("`{other}` cannot be undone")),
        };
        match step {
            Step::Restored => {
                index.mark_undone(a.id)?;
                out.restored.push((a.root.clone(), rel(&a.src)));
            }
            Step::NeverMoved => index.mark_undone(a.id)?,
            Step::Failed(reason) => out.failed.push((a.src.clone(), reason)),
            Step::Gone(reason) => {
                index.mark_undone(a.id)?;
                out.failed.push((a.src.clone(), format!("{reason}; it cannot be undone")));
            }
        }
    }
    for (batch_dir, rels) in quarantined {
        quarantine::forget_files(&batch_dir, &rels)?;
    }
    Ok(out)
}

/// The quarantine folder of `batch`: named `<batch>` or `<batch>-<root tag>`.
fn is_batch_dir(dir: &Path, batch: &str) -> bool {
    dir.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.strip_prefix(batch).is_some_and(|rest| rest.is_empty() || rest.starts_with('-')))
}

#[cfg(any(
    target_os = "windows",
    all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))
))]
fn restore_from_trash(original: &Path) -> Step {
    let items = match trash::os_limited::list() {
        Ok(items) => items,
        Err(e) => return Step::Failed(e.to_string()),
    };
    let item = items.into_iter().filter(|i| i.original_path() == original).max_by_key(|i| i.time_deleted);
    let occupied = std::fs::symlink_metadata(original).is_ok();
    match item {
        None if occupied => Step::NeverMoved,
        None => Step::Gone("no longer in the trash".into()),
        Some(_) if occupied => Step::Failed(format!("{} is occupied", original.display())),
        Some(item) => match trash::os_limited::restore_all([item]) {
            Ok(()) => Step::Restored,
            Err(e) => Step::Failed(e.to_string()),
        },
    }
}

#[cfg(not(any(
    target_os = "windows",
    all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))
)))]
fn restore_from_trash(_original: &Path) -> Step {
    Step::Gone("restore it from the system trash yourself".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn setup(files: &[&str]) -> (tempfile::TempDir, Index, Vec<FileEntry>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut index = Index::open_in_memory().unwrap();
        let row = index.ensure_root(&root).unwrap();
        let mut rows = Vec::new();
        for f in files {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, f.as_bytes()).unwrap();
            let meta = std::fs::metadata(&p).unwrap();
            rows.push((f.to_string(), meta.len(), mtime_ns(&meta)));
        }
        let ids = index.upsert_files(row.id, &rows).unwrap();
        index.set_root_stats(row.id, device_of(&root), rows.len() as i64, 0).unwrap();
        let root = Arc::new(root);
        let entries = ids
            .iter()
            .zip(rows)
            .map(|(&id, (rel, size, mtime_ns))| FileEntry {
                id,
                root: root.clone(),
                rel,
                size,
                mtime_ns,
                fields: BTreeMap::new(),
            })
            .collect();
        (dir, index, entries)
    }

    fn quarantine_config(root: &Path) -> Config {
        let mut c = Config::default();
        c.roots.push(RootConfig { path: root.to_path_buf(), delete: Some("quarantine".into()), quarantine: None });
        c
    }

    #[test]
    fn last_copy_is_protected() {
        let (_d, index, e) = setup(&["a.mkv", "b.mkv", "c.mkv"]);
        let config = quarantine_config(&e[0].root);
        let groups = HashMap::from([("g".to_owned(), vec![e[0].clone(), e[1].clone()])]);
        // Removing both members of the group is refused; c is an only copy.
        let plan = Plan::new(&index, &config, e.clone(), groups.clone()).unwrap();
        assert!(plan.items[0].refused.as_deref().unwrap().contains("last remaining copy"));
        assert!(plan.items[1].refused.is_some());
        assert!(plan.items[2].only_copy && !plan.items[2].include);
        assert_eq!(plan.totals().0, 0);
        // Removing one of the two is fine.
        let plan = Plan::new(&index, &config, vec![e[1].clone()], groups).unwrap();
        assert_eq!(plan.totals().0, 1);
    }

    #[test]
    fn survivor_must_still_exist() {
        let (_d, index, e) = setup(&["a.mkv", "b.mkv"]);
        let config = quarantine_config(&e[0].root);
        std::fs::remove_file(e[0].abs()).unwrap();
        let groups = HashMap::from([("g".to_owned(), e.clone())]);
        let plan = Plan::new(&index, &config, vec![e[1].clone()], groups).unwrap();
        assert!(plan.items[0].refused.as_deref().unwrap().contains("last remaining copy"));
    }

    #[test]
    fn quarantine_execute_and_undo() {
        let (_d, mut index, e) = setup(&["Show/a.mkv", "Show/b.mkv"]);
        let root = e[0].root.as_path().to_path_buf();
        let config = quarantine_config(&root);
        let groups = HashMap::from([("g".to_owned(), e.clone())]);
        let mut plan = Plan::new(&index, &config, vec![e[1].clone()], groups).unwrap();
        let out = execute(&mut index, &mut plan).unwrap();
        assert_eq!(out.done.len(), 1, "skipped: {:?}", out.skipped);
        assert!(!root.join("Show/b.mkv").exists());
        let moved = root.join(QUARANTINE_DIR).join(&out.batch).join("Show/b.mkv");
        assert!(moved.exists());
        let batches = quarantine::list(&[root.join(QUARANTINE_DIR)]).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].files, 1);

        assert_eq!(index.last_undoable_batch().unwrap().as_deref(), Some(out.batch.as_str()));
        let undo = undo_batch(&mut index, &out.batch).unwrap();
        assert_eq!(undo.restored, [(root.clone(), "Show/b.mkv".to_owned())]);
        assert!(root.join("Show/b.mkv").exists());
        assert!(!root.join(QUARANTINE_DIR).join(&out.batch).exists(), "empty batch dir is removed");
        assert_eq!(index.last_undoable_batch().unwrap(), None);
    }

    #[test]
    fn changed_files_are_skipped() {
        let (_d, mut index, e) = setup(&["a.mkv", "b.mkv"]);
        let config = quarantine_config(&e[0].root);
        std::fs::write(e[1].abs(), b"different length").unwrap();
        let groups = HashMap::from([("g".to_owned(), e.clone())]);
        let mut plan = Plan::new(&index, &config, vec![e[1].clone()], groups).unwrap();
        let out = execute(&mut index, &mut plan).unwrap();
        assert!(out.done.is_empty());
        assert!(out.skipped[0].1.contains("changed since the last scan"));
    }

    #[test]
    fn rename_and_undo() {
        let (_d, mut index, e) = setup(&["Show/old.mkv"]);
        let root = e[0].root.as_path().to_path_buf();
        let policy = root_policy(&index.roots().unwrap()[0], None);
        let new_rel = rename(&mut index, &policy, &e[0], "new.mkv").unwrap();
        assert_eq!(new_rel, "Show/new.mkv");
        assert!(root.join("Show/new.mkv").exists());
        let batch = index.last_undoable_batch().unwrap().unwrap();
        let undo = undo_batch(&mut index, &batch).unwrap();
        assert!(root.join("Show/old.mkv").exists());
        assert_eq!(undo.removed, [(root, "Show/new.mkv".to_owned())]);
    }

    #[test]
    fn misconfigured_trash_is_blocked_not_downgraded() {
        let row = RootRow {
            id: 1,
            path: PathBuf::from("/definitely/not/here"),
            device: None,
            file_count: 0,
            last_scan: None,
        };
        let cfg = RootConfig { path: row.path.clone(), delete: Some("trash".into()), quarantine: None };
        let p = root_policy(&row, Some(&cfg));
        assert_eq!(p.kind, StrategyKind::Trash);
        assert!(p.blocked.is_some());
    }

    #[test]
    fn manifest_and_journal_only_keep_what_moved() {
        let (_d, mut index, e) = setup(&["Show/a.mkv", "Show/b.mkv", "Show/c.mkv"]);
        let root = e[0].root.as_path().to_path_buf();
        let config = quarantine_config(&root);
        let groups = HashMap::from([("g".to_owned(), e.clone())]);
        let mut plan = Plan::new(&index, &config, vec![e[1].clone(), e[2].clone()], groups).unwrap();
        // c changes after the review: it is skipped and must not linger in
        // the manifest or the journal.
        std::fs::write(e[2].abs(), b"changed after review").unwrap();
        let out = execute(&mut index, &mut plan).unwrap();
        assert_eq!((out.done.len(), out.skipped.len()), (1, 1));
        let batches = quarantine::list(&[root.join(QUARANTINE_DIR)]).unwrap();
        assert_eq!(batches[0].files, 1);
        assert_eq!(index.actions(Some(&out.batch)).unwrap().len(), 1);
    }

    #[test]
    fn undo_skips_journal_entries_whose_file_never_moved() {
        // As left behind by a crash between journaling and moving.
        let (_d, mut index, e) = setup(&["a.mkv"]);
        let root = e[0].root.as_path().to_path_buf();
        let dst = root.join(QUARANTINE_DIR).join("b1").join("a.mkv");
        index.record_action("b1", "quarantine", "quarantine", &root, &e[0].abs(), Some(&dst), 1, 1).unwrap();
        let undo = undo_batch(&mut index, "b1").unwrap();
        assert!(undo.failed.is_empty() && undo.restored.is_empty());
        assert_eq!(index.last_undoable_batch().unwrap(), None, "the batch no longer blocks `u`");
    }

    #[test]
    fn files_on_another_device_than_their_root_are_refused() {
        let (_d, _index, e) = setup(&["a.mkv"]);
        let dev = device_of(&e[0].abs());
        assert!(check_file(&e[0], dev).is_ok());
        if let Some(d) = dev {
            assert!(check_file(&e[0], Some(d + 1)).unwrap_err().contains("another file system"));
        }
    }

    #[test]
    fn the_same_file_under_another_root_is_no_survivor() {
        let (_d, index, e) = setup(&["Show/a.mkv"]);
        let config = quarantine_config(&e[0].root);
        // As indexed through an overlapping root: same file, other entry.
        let parent = e[0].root.parent().unwrap().to_path_buf();
        let name = e[0].root.file_name().unwrap().to_str().unwrap();
        let alias = FileEntry { id: 99, root: Arc::new(parent), rel: format!("{name}/Show/a.mkv"), ..e[0].clone() };
        let groups = HashMap::from([("g".to_owned(), vec![e[0].clone(), alias])]);
        let plan = Plan::new(&index, &config, vec![e[0].clone()], groups).unwrap();
        assert!(plan.items[0].refused.as_deref().unwrap().contains("last remaining copy"));
    }

    #[test]
    fn a_shared_quarantine_dir_keeps_roots_apart() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let q = base.join("q");
        let mut index = Index::open_in_memory().unwrap();
        let mut config = Config::default();
        let mut entries = Vec::new();
        for r in ["r1", "r2"] {
            let root = base.join(r);
            std::fs::create_dir_all(&root).unwrap();
            config.roots.push(RootConfig {
                path: root.clone(),
                delete: Some("quarantine".into()),
                quarantine: Some(q.clone()),
            });
            let row = index.ensure_root(&root).unwrap();
            let mut rows = Vec::new();
            for f in ["x.mkv", "y.mkv"] {
                std::fs::write(root.join(f), r.as_bytes()).unwrap();
                let meta = std::fs::metadata(root.join(f)).unwrap();
                rows.push((f.to_owned(), meta.len(), mtime_ns(&meta)));
            }
            let ids = index.upsert_files(row.id, &rows).unwrap();
            index.set_root_stats(row.id, device_of(&root), 2, 0).unwrap();
            let root = Arc::new(root);
            for (id, (rel, size, mtime_ns)) in ids.into_iter().zip(rows) {
                entries.push(FileEntry { id, root: root.clone(), rel, size, mtime_ns, fields: BTreeMap::new() });
            }
        }
        // x.mkv of both roots goes, in one batch; both have the same rel path.
        let groups = HashMap::from([
            ("g1".to_owned(), vec![entries[0].clone(), entries[1].clone()]),
            ("g2".to_owned(), vec![entries[2].clone(), entries[3].clone()]),
        ]);
        let mut plan = Plan::new(&index, &config, vec![entries[0].clone(), entries[2].clone()], groups).unwrap();
        let out = execute(&mut index, &mut plan).unwrap();
        assert_eq!(out.done.len(), 2, "skipped: {:?}", out.skipped);
        let batches = quarantine::list(&[q]).unwrap();
        assert_eq!(batches.len(), 2);
        assert!(batches.iter().all(|b| b.id == out.batch && b.files == 1));
        for b in &batches {
            quarantine::restore(b).unwrap();
            assert_eq!(std::fs::read(b.root.join("x.mkv")).unwrap(), b.root.file_name().unwrap().as_encoded_bytes());
        }
    }

    #[test]
    fn undo_retires_actions_that_can_never_be_undone() {
        let (_d, mut index, e) = setup(&["a.mkv", "b.mkv"]);
        let root = e[0].root.as_path().to_path_buf();
        let config = quarantine_config(&root);
        let groups = HashMap::from([("g".to_owned(), e.clone())]);
        let mut plan = Plan::new(&index, &config, vec![e[1].clone()], groups).unwrap();
        let out = execute(&mut index, &mut plan).unwrap();
        std::fs::remove_file(root.join(QUARANTINE_DIR).join(&out.batch).join("b.mkv")).unwrap();
        let undo = undo_batch(&mut index, &out.batch).unwrap();
        assert!(undo.failed[0].1.contains("cannot be undone"));
        assert_eq!(index.last_undoable_batch().unwrap(), None, "the batch no longer blocks `u`");
    }

    #[test]
    fn purging_quarantine_keeps_other_actions_undoable() {
        let (_d, index, e) = setup(&["a.mkv"]);
        let root = e[0].root.as_path().to_path_buf();
        let qdir = root.join(QUARANTINE_DIR).join("b1");
        index.record_action("b1", "trash", "trash", &root, &e[0].abs(), None, 1, 1).unwrap();
        index
            .record_action("b1", "quarantine", "quarantine", &root, &e[0].abs(), Some(&qdir.join("a.mkv")), 1, 1)
            .unwrap();
        index.retire_quarantine("b1", &qdir).unwrap();
        let pending: Vec<_> = index.actions(Some("b1")).unwrap().into_iter().filter(|a| !a.undone).collect();
        assert_eq!(pending.iter().map(|a| a.op.as_str()).collect::<Vec<_>>(), ["trash"]);
    }

    #[cfg(unix)]
    #[test]
    fn moves_never_replace_anything() {
        let (_d, mut index, e) = setup(&["a.mkv"]);
        let root = e[0].root.as_path().to_path_buf();
        std::os::unix::fs::symlink(root.join("nowhere"), root.join("b.mkv")).unwrap();
        let err = move_no_copy(&e[0].abs(), &root.join("b.mkv")).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        let policy = root_policy(&index.roots().unwrap()[0], None);
        assert!(rename(&mut index, &policy, &e[0], "b.mkv").is_err());
        assert!(e[0].abs().exists());
        assert!(index.actions(None).unwrap().is_empty(), "a failed rename leaves no journal entry");
    }

    #[test]
    fn toggling_uses_cached_liveness_and_execute_refreshes() {
        let (_d, mut index, e) = setup(&["a.mkv", "b.mkv"]);
        let config = quarantine_config(&e[0].root);
        let groups = HashMap::from([("g".to_owned(), e.clone())]);
        let mut plan = Plan::new(&index, &config, vec![e[1].clone()], groups).unwrap();
        std::fs::remove_file(e[0].abs()).unwrap(); // the survivor vanishes
        plan.toggle(0);
        plan.toggle(0);
        assert_eq!(plan.totals().0, 1, "no re-stat on toggle");
        let out = execute(&mut index, &mut plan).unwrap();
        assert!(out.done.is_empty(), "execute re-checks the survivor");
        assert!(e[1].abs().exists());
    }
}
