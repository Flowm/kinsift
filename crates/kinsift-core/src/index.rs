//! The SQLite index: files, provider output, resolved fields and the action journal.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};

use crate::error::{Error, Result};
use crate::filter::Filter;
use crate::provider::{Extraction, Priority};
use crate::value::Value;

pub type FileId = i64;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS roots (
    id         INTEGER PRIMARY KEY,
    path       TEXT NOT NULL UNIQUE,
    device     INTEGER,
    file_count INTEGER NOT NULL DEFAULT 0,
    last_scan  INTEGER
);
CREATE TABLE IF NOT EXISTS files (
    id       INTEGER PRIMARY KEY,
    root_id  INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,
    size     INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    UNIQUE (root_id, rel_path)
);
-- One row per (file, provider): raw output, version, errors.
CREATE TABLE IF NOT EXISTS extractions (
    file_id  INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    version  INTEGER NOT NULL,
    raw      TEXT,
    error    TEXT,
    PRIMARY KEY (file_id, provider)
);
-- Every value every provider produced.
CREATE TABLE IF NOT EXISTS field_values (
    file_id  INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    key      TEXT NOT NULL,
    kind     TEXT NOT NULL,
    int_val  INTEGER,
    real_val REAL,
    text_val TEXT,
    PRIMARY KEY (file_id, provider, key)
);
-- The winning value per (file, key) after applying provider priority.
CREATE TABLE IF NOT EXISTS fields (
    file_id  INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    key      TEXT NOT NULL,
    provider TEXT NOT NULL,
    kind     TEXT NOT NULL,
    int_val  INTEGER,
    real_val REAL,
    text_val TEXT,
    PRIMARY KEY (file_id, key)
);
CREATE INDEX IF NOT EXISTS fields_key_int  ON fields (key, int_val);
CREATE INDEX IF NOT EXISTS fields_key_text ON fields (key, text_val);
-- Journal of every change kinsift made on disk.
CREATE TABLE IF NOT EXISTS actions (
    id       INTEGER PRIMARY KEY,
    batch    TEXT NOT NULL,
    time     INTEGER NOT NULL,
    op       TEXT NOT NULL,
    strategy TEXT NOT NULL,
    root     TEXT NOT NULL,
    src      TEXT NOT NULL,
    dst      TEXT,
    size     INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    undone   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS actions_batch ON actions (batch);
"#;

#[derive(Debug, Clone)]
pub struct RootRow {
    pub id: i64,
    pub path: PathBuf,
    pub device: Option<u64>,
    pub file_count: i64,
    pub last_scan: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct StoredFile {
    pub id: FileId,
    pub size: u64,
    pub mtime_ns: i64,
}

/// A file with its resolved fields, as used by views and the TUI.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub id: FileId,
    pub root: Arc<PathBuf>,
    pub rel: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub fields: BTreeMap<String, Value>,
}

impl FileEntry {
    pub fn abs(&self) -> PathBuf {
        self.root.join(&self.rel)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }
}

/// The result of running one provider on one file.
pub struct ExtractionWrite {
    pub file_id: FileId,
    pub provider: &'static str,
    pub version: u32,
    pub outcome: std::result::Result<Option<Extraction>, String>,
}

/// One provider's value for one field, for `inspect`.
#[derive(Debug, Clone)]
pub struct SourceRow {
    pub key: String,
    pub provider: String,
    pub value: Value,
    pub effective: bool,
}

#[derive(Debug, Clone)]
pub struct ExtractionRow {
    pub provider: String,
    pub version: u32,
    pub raw: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ActionRow {
    pub id: i64,
    pub batch: String,
    pub time: i64,
    pub op: String,
    pub strategy: String,
    pub root: PathBuf,
    pub src: PathBuf,
    pub dst: Option<PathBuf>,
    pub size: u64,
    pub mtime_ns: i64,
    pub undone: bool,
}

pub struct Index {
    conn: Connection,
}

impl Index {
    pub fn open(path: &Path) -> Result<Index> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Index> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Index> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(Error::Other(format!(
                "index schema version {version} is newer than this kinsift supports ({SCHEMA_VERSION})"
            )));
        }
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Index { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // ---- meta ---------------------------------------------------------------

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", [key, value])?;
        Ok(())
    }

    // ---- roots --------------------------------------------------------------

    pub fn roots(&self) -> Result<Vec<RootRow>> {
        let mut stmt = self.conn.prepare("SELECT id, path, device, file_count, last_scan FROM roots ORDER BY path")?;
        let rows = stmt.query_map([], root_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn root_by_path(&self, path: &Path) -> Result<Option<RootRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path, device, file_count, last_scan FROM roots WHERE path = ?1",
                [path_str(path)],
                root_row,
            )
            .optional()?)
    }

    pub fn ensure_root(&self, path: &Path) -> Result<RootRow> {
        self.conn.execute("INSERT OR IGNORE INTO roots (path) VALUES (?1)", [path_str(path)])?;
        self.root_by_path(path)?.ok_or_else(|| Error::Other("root vanished".into()))
    }

    pub fn set_root_stats(&self, id: i64, device: Option<u64>, file_count: i64, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE roots SET device = ?2, file_count = ?3, last_scan = ?4 WHERE id = ?1",
            params![id, device.map(|d| d as i64), file_count, now],
        )?;
        Ok(())
    }

    pub fn remove_root(&self, path: &Path) -> Result<bool> {
        Ok(self.conn.execute("DELETE FROM roots WHERE path = ?1", [path_str(path)])? > 0)
    }

    // ---- files --------------------------------------------------------------

    pub fn stored_files(&self, root_id: i64) -> Result<HashMap<String, StoredFile>> {
        let mut stmt = self.conn.prepare("SELECT id, rel_path, size, mtime_ns FROM files WHERE root_id = ?1")?;
        let rows = stmt.query_map([root_id], |r| {
            Ok((
                r.get::<_, String>(1)?,
                StoredFile { id: r.get(0)?, size: r.get::<_, i64>(2)? as u64, mtime_ns: r.get(3)? },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Provider versions that already ran, per file of a root.
    pub fn stored_extractions(&self, root_id: i64) -> Result<HashMap<FileId, HashMap<String, u32>>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.file_id, e.provider, e.version FROM extractions e JOIN files f ON f.id = e.file_id WHERE f.root_id = ?1",
        )?;
        let mut out: HashMap<FileId, HashMap<String, u32>> = HashMap::new();
        let rows =
            stmt.query_map([root_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, u32>(2)?)))?;
        for row in rows {
            let (id, provider, version) = row?;
            out.entry(id).or_default().insert(provider, version);
        }
        Ok(out)
    }

    /// Insert or update files; returns their ids in input order.
    /// A changed size or mtime drops all previous provider output.
    pub fn upsert_files(&mut self, root_id: i64, files: &[(String, u64, i64)]) -> Result<Vec<FileId>> {
        let tx = self.conn.transaction()?;
        let mut ids = Vec::with_capacity(files.len());
        {
            let mut select =
                tx.prepare_cached("SELECT id, size, mtime_ns FROM files WHERE root_id = ?1 AND rel_path = ?2")?;
            let mut insert =
                tx.prepare_cached("INSERT INTO files (root_id, rel_path, size, mtime_ns) VALUES (?1, ?2, ?3, ?4)")?;
            let mut update = tx.prepare_cached("UPDATE files SET size = ?2, mtime_ns = ?3 WHERE id = ?1")?;
            let mut clear_ex = tx.prepare_cached("DELETE FROM extractions WHERE file_id = ?1")?;
            let mut clear_fv = tx.prepare_cached("DELETE FROM field_values WHERE file_id = ?1")?;
            let mut clear_f = tx.prepare_cached("DELETE FROM fields WHERE file_id = ?1")?;
            for (rel, size, mtime) in files {
                let existing: Option<(i64, i64, i64)> =
                    select.query_row(params![root_id, rel], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
                let id = match existing {
                    Some((id, s, m)) => {
                        if s as u64 != *size || m != *mtime {
                            update.execute(params![id, *size as i64, mtime])?;
                            clear_ex.execute([id])?;
                            clear_fv.execute([id])?;
                            clear_f.execute([id])?;
                        }
                        id
                    }
                    None => {
                        insert.execute(params![root_id, rel, *size as i64, mtime])?;
                        tx.last_insert_rowid()
                    }
                };
                ids.push(id);
            }
        }
        tx.commit()?;
        Ok(ids)
    }

    pub fn delete_files(&mut self, ids: &[FileId]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached("DELETE FROM files WHERE id = ?1")?;
            for id in ids {
                stmt.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Forget previous provider output so the next extraction re-runs everything.
    pub fn clear_extractions(&mut self, ids: &[FileId]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for id in ids {
            tx.execute("DELETE FROM extractions WHERE file_id = ?1", [id])?;
            tx.execute("DELETE FROM field_values WHERE file_id = ?1", [id])?;
            tx.execute("DELETE FROM fields WHERE file_id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove the entry for a relative path in a root, if present.
    pub fn delete_by_rel(&self, root: &Path, rel: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM files WHERE rel_path = ?2 AND root_id = (SELECT id FROM roots WHERE path = ?1)",
            params![path_str(root), rel],
        )?;
        Ok(())
    }

    /// Move a file's index entry after a rename on disk.
    pub fn rename_file(&self, id: FileId, new_rel: &str) -> Result<()> {
        self.conn.execute("UPDATE files SET rel_path = ?2 WHERE id = ?1", params![id, new_rel])?;
        Ok(())
    }

    // ---- extraction results -------------------------------------------------

    pub fn write_extractions(&mut self, items: Vec<ExtractionWrite>, priority: &Priority) -> Result<()> {
        let tx = self.conn.transaction()?;
        let mut touched = HashSet::new();
        {
            let mut del = tx.prepare_cached("DELETE FROM field_values WHERE file_id = ?1 AND provider = ?2")?;
            let mut exists = tx.prepare_cached("SELECT 1 FROM files WHERE id = ?1")?;
            let mut ins_ex = tx.prepare_cached(
                "INSERT OR REPLACE INTO extractions (file_id, provider, version, raw, error) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let mut ins_fv = tx.prepare_cached(
                "INSERT OR REPLACE INTO field_values (file_id, provider, key, kind, int_val, real_val, text_val)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for item in items {
                // The file may have been removed (by another connection)
                // while the provider ran; its results are moot then.
                if !exists.exists([item.file_id])? {
                    continue;
                }
                del.execute(params![item.file_id, item.provider])?;
                match item.outcome {
                    Ok(extraction) => {
                        let extraction = extraction.unwrap_or_default();
                        let raw = extraction.raw.as_ref().map(|r| r.to_string());
                        ins_ex.execute(params![item.file_id, item.provider, item.version, raw, None::<String>])?;
                        for set in &extraction.sets {
                            for (key, value) in set.keys() {
                                let (kind, i, r, t) = value.to_sql();
                                ins_fv.execute(params![item.file_id, item.provider, key, kind, i, r, t])?;
                            }
                        }
                    }
                    Err(err) => {
                        ins_ex.execute(params![
                            item.file_id,
                            item.provider,
                            item.version,
                            None::<String>,
                            Some(err)
                        ])?;
                    }
                }
                touched.insert(item.file_id);
            }
        }
        for id in touched {
            resolve_file(&tx, id, priority)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Re-apply provider priority to every file (after a config change).
    pub fn resolve_all(&mut self, priority: &Priority) -> Result<()> {
        let tx = self.conn.transaction()?;
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM files")?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            resolve_file(&tx, id, priority)?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- reading ------------------------------------------------------------

    /// Load files (optionally filtered) with all their resolved fields.
    pub fn load_entries(&self, filter: Option<&Filter>, root_id: Option<i64>) -> Result<Vec<FileEntry>> {
        let mut where_sql = String::from("1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(root_id) = root_id {
            where_sql.push_str(" AND files.root_id = ?");
            args.push(root_id.into());
        }
        if let Some(filter) = filter {
            let (sql, mut fargs) = filter.to_sql();
            where_sql.push_str(" AND ");
            where_sql.push_str(&sql);
            args.append(&mut fargs);
        }

        let mut roots: HashMap<i64, Arc<PathBuf>> = HashMap::new();
        for root in self.roots()? {
            roots.insert(root.id, Arc::new(root.path));
        }

        let mut entries: Vec<FileEntry> = Vec::new();
        let mut by_id: HashMap<FileId, usize> = HashMap::new();
        {
            let sql = format!(
                "SELECT files.id, files.root_id, files.rel_path, files.size, files.mtime_ns FROM files WHERE {where_sql}"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(args.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?;
            for row in rows {
                let (id, root_id, rel, size, mtime_ns) = row?;
                let Some(root) = roots.get(&root_id) else { continue };
                by_id.insert(id, entries.len());
                entries.push(FileEntry {
                    id,
                    root: root.clone(),
                    rel,
                    size: size as u64,
                    mtime_ns,
                    fields: BTreeMap::new(),
                });
            }
        }
        {
            let sql = format!(
                "SELECT fields.file_id, fields.key, fields.kind, fields.int_val, fields.real_val, fields.text_val
                 FROM fields JOIN files ON files.id = fields.file_id WHERE {where_sql}"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(args.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<f64>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })?;
            for row in rows {
                let (id, key, kind, i, f, t) = row?;
                if let (Some(&idx), Some(value)) = (by_id.get(&id), Value::from_sql(&kind, i, f, t)) {
                    entries[idx].fields.insert(key, value);
                }
            }
        }
        Ok(entries)
    }

    pub fn entry(&self, id: FileId) -> Result<Option<FileEntry>> {
        let filter = Filter::by_id(id);
        Ok(self.load_entries(Some(&filter), None)?.into_iter().next())
    }

    /// Find the indexed file at an absolute path.
    pub fn find_by_path(&self, abs: &Path) -> Result<Option<FileEntry>> {
        let abs = abs.canonicalize().unwrap_or_else(|_| abs.to_path_buf());
        let mut best: Option<RootRow> = None;
        for root in self.roots()? {
            if abs.starts_with(&root.path)
                && best.as_ref().is_none_or(|b| root.path.as_os_str().len() > b.path.as_os_str().len())
            {
                best = Some(root);
            }
        }
        let Some(root) = best else { return Ok(None) };
        let rel = rel_string(abs.strip_prefix(&root.path).unwrap_or(&abs));
        let id: Option<i64> = self
            .conn
            .query_row("SELECT id FROM files WHERE root_id = ?1 AND rel_path = ?2", params![root.id, rel], |r| r.get(0))
            .optional()?;
        match id {
            Some(id) => self.entry(id),
            None => Ok(None),
        }
    }

    pub fn sources(&self, id: FileId) -> Result<Vec<SourceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT v.key, v.provider, v.kind, v.int_val, v.real_val, v.text_val,
                    EXISTS (SELECT 1 FROM fields f WHERE f.file_id = v.file_id AND f.key = v.key AND f.provider = v.provider)
             FROM field_values v WHERE v.file_id = ?1 ORDER BY v.key, v.provider",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<f64>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, bool>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, provider, kind, i, f, t, effective) = row?;
            if let Some(value) = Value::from_sql(&kind, i, f, t) {
                out.push(SourceRow { key, provider, value, effective });
            }
        }
        Ok(out)
    }

    pub fn extractions(&self, id: FileId) -> Result<Vec<ExtractionRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT provider, version, raw, error FROM extractions WHERE file_id = ?1 ORDER BY provider")?;
        let rows = stmt.query_map([id], |r| {
            Ok(ExtractionRow { provider: r.get(0)?, version: r.get(1)?, raw: r.get(2)?, error: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Number of files that have each resolved field.
    pub fn field_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare("SELECT key, COUNT(*) FROM fields GROUP BY key ORDER BY key")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- action journal -----------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn record_action(
        &self,
        batch: &str,
        op: &str,
        strategy: &str,
        root: &Path,
        src: &Path,
        dst: Option<&Path>,
        size: u64,
        mtime_ns: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO actions (batch, time, op, strategy, root, src, dst, size, mtime_ns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                batch,
                now_unix(),
                op,
                strategy,
                path_str(root),
                path_str(src),
                dst.map(path_str),
                size as i64,
                mtime_ns
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn actions(&self, batch: Option<&str>) -> Result<Vec<ActionRow>> {
        let (sql, args): (&str, Vec<String>) = match batch {
            Some(b) => ("SELECT * FROM actions WHERE batch = ?1 ORDER BY id", vec![b.to_owned()]),
            None => ("SELECT * FROM actions ORDER BY id", vec![]),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |r| {
            Ok(ActionRow {
                id: r.get("id")?,
                batch: r.get("batch")?,
                time: r.get("time")?,
                op: r.get("op")?,
                strategy: r.get("strategy")?,
                root: PathBuf::from(r.get::<_, String>("root")?),
                src: PathBuf::from(r.get::<_, String>("src")?),
                dst: r.get::<_, Option<String>>("dst")?.map(PathBuf::from),
                size: r.get::<_, i64>("size")? as u64,
                mtime_ns: r.get("mtime_ns")?,
                undone: r.get("undone")?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The most recent batch that still has actions which were not undone.
    pub fn last_undoable_batch(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT batch FROM actions WHERE undone = 0 AND op IN ('trash', 'quarantine', 'rename')
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Files purged from a quarantine batch folder can no longer be undone.
    /// Only the quarantine actions into that folder are retired: one batch
    /// can span several roots and strategies.
    pub fn retire_quarantine(&self, batch: &str, batch_dir: &Path) -> Result<()> {
        let Some(name) = batch_dir.file_name().and_then(|n| n.to_str()) else { return Ok(()) };
        self.conn.execute(
            "UPDATE actions SET undone = 1
             WHERE batch = ?1 AND op = 'quarantine' AND (instr(dst, '/' || ?2 || '/') > 0 OR instr(dst, '\\' || ?2 || '\\') > 0)",
            params![batch, name],
        )?;
        Ok(())
    }

    /// Remove a journal entry for an action that did not happen.
    pub fn delete_action(&self, action_id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM actions WHERE id = ?1", [action_id])?;
        Ok(())
    }

    pub fn mark_undone(&self, action_id: i64) -> Result<()> {
        self.conn.execute("UPDATE actions SET undone = 1 WHERE id = ?1", [action_id])?;
        Ok(())
    }
}

/// Pick the winning value per key for one file and record conflicts.
fn resolve_file(tx: &Transaction, file_id: FileId, priority: &Priority) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "SELECT key, provider, kind, int_val, real_val, text_val FROM field_values WHERE file_id = ?1",
    )?;
    let rows = stmt.query_map([file_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<f64>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut by_key: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
    for row in rows {
        let (key, provider, kind, i, f, t) = row?;
        if let Some(value) = Value::from_sql(&kind, i, f, t) {
            by_key.entry(key).or_default().push((provider, value));
        }
    }
    drop(stmt);

    tx.prepare_cached("DELETE FROM fields WHERE file_id = ?1")?.execute([file_id])?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO fields (file_id, key, provider, kind, int_val, real_val, text_val) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut conflicts = Vec::new();
    for (key, candidates) in &by_key {
        let (provider, value) =
            candidates.iter().max_by_key(|(p, _)| priority.rank(key, p)).expect("at least one candidate");
        let distinct: HashSet<String> = candidates.iter().map(|(_, v)| v.group_key()).collect();
        if distinct.len() > 1 {
            conflicts.push(Value::Str(key.clone()));
        }
        let (kind, i, f, t) = value.to_sql();
        insert.execute(params![file_id, key, provider, kind, i, f, t])?;
    }
    if !conflicts.is_empty() {
        let (kind, i, f, t) = Value::List(conflicts).to_sql();
        insert.execute(params![file_id, "meta.conflicts", "kinsift", kind, i, f, t])?;
    }
    Ok(())
}

fn root_row(r: &rusqlite::Row) -> rusqlite::Result<RootRow> {
    Ok(RootRow {
        id: r.get(0)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        device: r.get::<_, Option<i64>>(2)?.map(|d| d as u64),
        file_count: r.get(3)?,
        last_scan: r.get(4)?,
    })
}

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Relative path as stored in the index: `/`-separated.
pub fn rel_string(p: &Path) -> String {
    p.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_for_removed_files_are_dropped() {
        let mut index = Index::open_in_memory().unwrap();
        let root = index.ensure_root(Path::new("/r")).unwrap();
        let ids = index.upsert_files(root.id, &[("a".into(), 1, 1), ("b".into(), 1, 1)]).unwrap();
        index.delete_files(&ids[..1]).unwrap(); // removed while a provider ran
        let write = |id| ExtractionWrite { file_id: id, provider: "hash", version: 1, outcome: Ok(None) };
        index.write_extractions(vec![write(ids[0]), write(ids[1])], &Priority::default()).unwrap();
        assert_eq!(index.extractions(ids[1]).unwrap().len(), 1);
    }
}
