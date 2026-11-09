//! TUI state and key handling (the "update" half of the Elm-style loop).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use kinsift_core::actions::{self, Plan, StrategyKind};
use kinsift_core::config::{Config, View, parse_size};
use kinsift_core::filter::Filter;
use kinsift_core::index::{FileEntry, FileId, Index, SourceRow};
use kinsift_core::keep::KeepRule;
use kinsift_core::provider::{Priority, Provider};
use kinsift_core::quarantine;
use kinsift_core::query::{Node, SortBy, build_tree, sort_tree};
use kinsift_core::scan::{ScanEvent, refresh_files};
use kinsift_core::value::{Value, format_size};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Everything the TUI needs from the outside.
pub struct TuiContext {
    pub config: Config,
    pub index: Index,
    pub index_path: PathBuf,
    pub providers: Arc<Vec<Box<dyn Provider>>>,
    pub priority: Priority,
    pub start_view: Option<String>,
}

#[derive(Debug, Clone)]
pub enum RowKind {
    /// Path of child indices from the root.
    Group(Vec<usize>),
    File(usize),
}

#[derive(Debug, Clone)]
pub struct Row {
    pub depth: usize,
    pub kind: RowKind,
    /// For each level from the top down to this row: is that node the last
    /// of its siblings? Drives the tree guide lines.
    pub last: Vec<bool>,
}

/// Distinct quality values below a group, shown next to its label.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub resolutions: BTreeSet<i64>,
    pub codecs: BTreeSet<String>,
}

impl Summary {
    pub fn of(entry: &FileEntry) -> Summary {
        let mut s = Summary::default();
        s.add(entry);
        s
    }

    fn add(&mut self, entry: &FileEntry) {
        if let Some(r) = entry.get("video.resolution").and_then(|v| v.as_f64()) {
            self.resolutions.insert(r as i64);
        }
        if let Some(c) = entry.get("video.codec").and_then(|v| v.as_str()) {
            self.codecs.insert(c.to_owned());
        }
    }

    fn merge(&mut self, other: &Summary) {
        self.resolutions.extend(&other.resolutions);
        self.codecs.extend(other.codecs.iter().cloned());
    }
}

pub enum PickerKind {
    View,
    Sort,
    KeepRule { scope: Vec<usize> },
    MarkByValue { options: Vec<(String, Vec<usize>)> },
}

pub enum InputKind {
    Filter,
    GroupBy,
    Rename(usize),
    ConfirmPermanent,
}

pub struct ReviewState {
    pub plan: Plan,
    pub cursor: usize,
}

pub enum Modal {
    Help,
    Picker { title: String, items: Vec<String>, cursor: usize, kind: PickerKind },
    Input { title: String, text: String, kind: InputKind },
    Review(ReviewState),
    ConfirmUndo { batch: String, count: usize },
    Message { title: String, lines: Vec<String> },
}

pub struct ScanJob {
    pub rx: Receiver<ScanMsg>,
    pub progress: String,
}

pub enum ScanMsg {
    Event(ScanEvent),
    Progress(String),
    Finished(Result<Vec<String>, String>),
}

pub struct App {
    pub ctx: TuiContext,
    pub view_idx: usize,
    pub group_override: Option<Vec<String>>,
    pub filter_extra: Option<String>,
    pub dup_only: bool,
    pub sort: SortBy,
    pub entries: Vec<FileEntry>,
    pub tree: Node,
    /// File index -> id of its duplicate group.
    pub dup_of: HashMap<usize, String>,
    /// Duplicate group id -> its files.
    pub dup_members: HashMap<String, Vec<usize>>,
    /// Node id -> quality summary of its subtree.
    pub summaries: HashMap<String, Summary>,
    pub expanded: HashSet<String>,
    pub rows: Vec<Row>,
    pub cursor: usize,
    pub offset: usize,
    pub height: usize,
    pub marked: BTreeSet<FileId>,
    pub detail: bool,
    pub detail_cache: Option<(FileId, Vec<SourceRow>)>,
    pub modal: Option<Modal>,
    pub status: Option<(String, Instant)>,
    pub scan: Option<ScanJob>,
    pub quarantine_note: Option<String>,
    pub review_pending: Option<Plan>,
    /// Set by `R`; the event loop starts a background scan.
    pub request_rescan: bool,
    /// Set by `H`; the event loop hashes the current view in the background.
    pub request_hash: bool,
    pub quit: bool,
}

impl App {
    pub fn new(ctx: TuiContext) -> anyhow::Result<App> {
        let view_idx =
            ctx.start_view.as_deref().and_then(|n| ctx.config.views.iter().position(|v| v.name == n)).unwrap_or(0);
        let mut app = App {
            ctx,
            view_idx,
            group_override: None,
            filter_extra: None,
            dup_only: false,
            sort: SortBy::Name,
            entries: Vec::new(),
            tree: Node::default(),
            dup_of: HashMap::new(),
            dup_members: HashMap::new(),
            summaries: HashMap::new(),
            expanded: HashSet::new(),
            rows: Vec::new(),
            cursor: 0,
            offset: 0,
            height: 20,
            marked: BTreeSet::new(),
            detail: false,
            detail_cache: None,
            modal: None,
            status: None,
            scan: None,
            quarantine_note: None,
            review_pending: None,
            request_rescan: false,
            request_hash: false,
            quit: false,
        };
        app.dup_only = app.base_view().duplicates_only;
        app.reload()?;
        app.refresh_quarantine_note();
        Ok(app)
    }

    pub fn base_view(&self) -> &View {
        &self.ctx.config.views[self.view_idx]
    }

    /// The view as currently configured, including overrides.
    pub fn effective_view(&self) -> View {
        let mut v = self.base_view().clone();
        if let Some(g) = &self.group_override {
            v.group_by = g.clone();
        }
        if let Some(extra) = &self.filter_extra {
            v.filter = Some(match &v.filter {
                Some(f) => format!("{f} and {extra}"),
                None => extra.clone(),
            });
        }
        v.duplicates_only = self.dup_only;
        v
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// Reload entries from the index and rebuild the tree.
    pub fn reload(&mut self) -> anyhow::Result<()> {
        let view = self.effective_view();
        let filter = view.filter.as_deref().map(Filter::parse).transpose()?;
        let key = self.cursor_key();
        self.entries = self.ctx.index.load_entries(filter.as_ref(), None)?;
        let known: HashSet<FileId> = self.entries.iter().map(|e| e.id).collect();
        self.marked.retain(|id| known.contains(id));
        // Dialogs that hold tree paths or entry indices would now point at
        // the wrong files.
        if matches!(
            self.modal,
            Some(Modal::Picker { kind: PickerKind::KeepRule { .. } | PickerKind::MarkByValue { .. }, .. })
                | Some(Modal::Input { kind: InputKind::Rename(_), .. })
        ) {
            self.modal = None;
            self.set_status("the view changed; open the dialog again");
        }
        self.rebuild_keeping(key);
        Ok(())
    }

    /// Rebuild the tree from the loaded entries (after grouping/sort changes).
    pub fn rebuild(&mut self) {
        let key = self.cursor_key();
        self.rebuild_keeping(key);
    }

    /// Rebuild the tree, then put the cursor back on the row identified by
    /// `key` (computed against the old tree and entries) if it still exists.
    fn rebuild_keeping(&mut self, key: Option<String>) {
        let view = self.effective_view();
        self.tree = build_tree(&self.entries, &view.group_by, view.duplicates_only);
        sort_tree(&mut self.tree, self.sort, &self.entries);
        self.dup_of.clear();
        self.dup_members.clear();
        fn collect(n: &Node, of: &mut HashMap<usize, String>, members: &mut HashMap<String, Vec<usize>>) {
            if n.duplicate {
                for &f in &n.files {
                    of.insert(f, n.id.clone());
                }
                members.insert(n.id.clone(), n.files.clone());
            }
            for c in &n.children {
                collect(c, of, members);
            }
        }
        collect(&self.tree, &mut self.dup_of, &mut self.dup_members);
        fn summarize(n: &Node, entries: &[FileEntry], out: &mut HashMap<String, Summary>) -> Summary {
            let mut s = Summary::default();
            for &f in &n.files {
                s.add(&entries[f]);
            }
            for c in &n.children {
                s.merge(&summarize(c, entries, out));
            }
            out.insert(n.id.clone(), s.clone());
            s
        }
        self.summaries.clear();
        summarize(&self.tree, &self.entries, &mut self.summaries);
        self.flatten_keeping(key);
    }

    pub fn node(&self, path: &[usize]) -> &Node {
        path.iter().fold(&self.tree, |n, &i| &n.children[i])
    }

    fn try_node(&self, path: &[usize]) -> Option<&Node> {
        path.iter().try_fold(&self.tree, |n, &i| n.children.get(i))
    }

    fn cursor_key(&self) -> Option<String> {
        self.rows.get(self.cursor).and_then(|r| self.row_key(r))
    }

    /// Recompute visible rows from the tree and expansion state.
    pub fn flatten(&mut self) {
        let key = self.cursor_key();
        self.flatten_keeping(key);
    }

    fn flatten_keeping(&mut self, cursor_key: Option<String>) {
        let mut rows = Vec::new();
        fn walk(app: &App, path: &mut Vec<usize>, last: &mut Vec<bool>, rows: &mut Vec<Row>) {
            let node = app.node(path);
            let depth = last.len();
            let total = node.children.len() + node.files.len();
            for i in 0..node.children.len() {
                path.push(i);
                last.push(i + 1 == total);
                rows.push(Row { depth, kind: RowKind::Group(path.clone()), last: last.clone() });
                if app.expanded.contains(&app.node(path).id) {
                    walk(app, path, last, rows);
                }
                last.pop();
                path.pop();
            }
            for (k, &f) in node.files.iter().enumerate() {
                last.push(node.children.len() + k + 1 == total);
                rows.push(Row { depth, kind: RowKind::File(f), last: last.clone() });
                last.pop();
            }
        }
        walk(self, &mut Vec::new(), &mut Vec::new(), &mut rows);
        self.rows = rows;
        if let Some(key) = cursor_key
            && let Some(pos) = self.rows.iter().position(|r| self.row_key(r).as_ref() == Some(&key))
        {
            self.cursor = pos;
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        self.clamp_scroll();
    }

    fn row_key(&self, r: &Row) -> Option<String> {
        match &r.kind {
            RowKind::Group(p) => self.try_node(p).map(|n| format!("g:{}", n.id)),
            RowKind::File(f) => self.entries.get(*f).map(|e| format!("f:{}", e.id)),
        }
    }

    fn clamp_scroll(&mut self) {
        let h = self.height.max(1);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + h {
            self.offset = self.cursor + 1 - h;
        }
    }

    fn current(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    /// The group path at the cursor (a file's parent group for file rows).
    fn current_group_path(&self) -> Vec<usize> {
        match self.current().map(|r| &r.kind) {
            Some(RowKind::Group(p)) => p.clone(),
            Some(RowKind::File(f)) => self.parent_path_of_file(*f).unwrap_or_default(),
            None => Vec::new(),
        }
    }

    fn parent_path_of_file(&self, file: usize) -> Option<Vec<usize>> {
        fn find(n: &Node, file: usize, path: &mut Vec<usize>) -> bool {
            if n.files.contains(&file) {
                return true;
            }
            for (i, c) in n.children.iter().enumerate() {
                path.push(i);
                if find(c, file, path) {
                    return true;
                }
                path.pop();
            }
            false
        }
        let mut path = Vec::new();
        find(&self.tree, file, &mut path).then_some(path)
    }

    pub fn current_file(&self) -> Option<&FileEntry> {
        match self.current().map(|r| &r.kind) {
            Some(RowKind::File(f)) => self.entries.get(*f),
            _ => None,
        }
    }

    // ---- input ------------------------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.modal.is_some() {
            self.on_modal_key(key);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-(self.height as isize)),
            KeyCode::PageDown => self.move_cursor(self.height as isize),
            KeyCode::Home | KeyCode::Char('g') => self.move_cursor(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_cursor(isize::MAX / 2),
            KeyCode::Right | KeyCode::Char('l') => self.expand(false),
            KeyCode::Left | KeyCode::Char('h') => self.collapse(),
            KeyCode::Enter => self.toggle_expand(),
            KeyCode::Char('e') => self.expand(true),
            KeyCode::Char('E') => self.expand_everything(),
            KeyCode::Char('c') => self.collapse_all_below(),
            KeyCode::Char('C') => {
                self.expanded.clear();
                self.flatten();
            }
            KeyCode::Char('f') => {
                self.dup_only = !self.dup_only;
                self.rebuild();
                self.set_status(if self.dup_only { "showing duplicate groups only" } else { "showing all groups" });
            }
            KeyCode::Char('s') => self.open_sort(),
            KeyCode::Char('v') => self.open_views(),
            KeyCode::Char('/') => {
                let text = self.filter_extra.clone().unwrap_or_default();
                self.modal = Some(Modal::Input {
                    title: "Filter (field op value and ...)".into(),
                    text,
                    kind: InputKind::Filter,
                });
            }
            KeyCode::Char('b') => {
                let text = self.effective_view().group_by.join(", ");
                self.modal = Some(Modal::Input {
                    title: "Group by (comma-separated fields)".into(),
                    text,
                    kind: InputKind::GroupBy,
                });
            }
            KeyCode::Char('i') | KeyCode::Tab => self.detail = !self.detail,
            KeyCode::Char('m') | KeyCode::Char(' ') => self.mark_at_cursor(),
            KeyCode::Char('u') if ctrl => {
                let n = self.marked.len();
                self.marked.clear();
                self.set_status(format!("unmarked {n} files"));
            }
            KeyCode::Char('K') => self.open_keep_rules(),
            KeyCode::Char('d') if ctrl => self.review_marked(),
            KeyCode::Char('d') | KeyCode::Delete => self.review_current_file(),
            KeyCode::Char('D') => self.review_marked(),
            KeyCode::Char('r') => self.open_rename(),
            KeyCode::Char('u') => self.ask_undo(),
            KeyCode::Char('R') => self.request_rescan = true,
            KeyCode::Char('H') => self.request_hash = true,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            _ => {}
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
        self.clamp_scroll();
    }

    fn expand(&mut self, recursive: bool) {
        let Some(RowKind::Group(path)) = self.current().map(|r| r.kind.clone()) else { return };
        let node = self.node(&path).clone();
        if recursive {
            fn all(n: &Node, out: &mut HashSet<String>) {
                out.insert(n.id.clone());
                n.children.iter().for_each(|c| all(c, out));
            }
            all(&node, &mut self.expanded);
        } else {
            self.expanded.insert(node.id);
        }
        self.flatten();
    }

    fn expand_everything(&mut self) {
        fn all(n: &Node, out: &mut HashSet<String>) {
            for c in &n.children {
                out.insert(c.id.clone());
                all(c, out);
            }
        }
        all(&self.tree, &mut self.expanded);
        self.flatten();
    }

    fn collapse(&mut self) {
        match self.current().map(|r| r.kind.clone()) {
            Some(RowKind::Group(path)) => {
                let id = self.node(&path).id.clone();
                if self.expanded.remove(&id) {
                    self.flatten();
                } else if path.len() > 1 {
                    self.jump_to_group(&path[..path.len() - 1]);
                }
            }
            Some(RowKind::File(f)) => {
                if let Some(parent) = self.parent_path_of_file(f).filter(|p| !p.is_empty()) {
                    self.jump_to_group(&parent);
                }
            }
            None => {}
        }
    }

    fn jump_to_group(&mut self, path: &[usize]) {
        if let Some(pos) = self.rows.iter().position(|r| matches!(&r.kind, RowKind::Group(p) if p == path)) {
            self.cursor = pos;
            self.clamp_scroll();
        }
    }

    fn toggle_expand(&mut self) {
        if let Some(RowKind::Group(path)) = self.current().map(|r| r.kind.clone()) {
            let id = self.node(&path).id.clone();
            if !self.expanded.remove(&id) {
                self.expanded.insert(id);
            }
            self.flatten();
        } else {
            self.detail = !self.detail;
        }
    }

    fn collapse_all_below(&mut self) {
        let path = self.current_group_path();
        let node = self.node(&path).clone();
        fn all(n: &Node, out: &mut HashSet<String>) {
            out.remove(&n.id);
            n.children.iter().for_each(|c| all(c, out));
        }
        all(&node, &mut self.expanded);
        self.flatten();
        if !path.is_empty() {
            self.jump_to_group(&path);
        }
    }

    fn open_sort(&mut self) {
        let items = SortBy::ALL.iter().map(|s| s.label().to_owned()).collect();
        let cursor = SortBy::ALL.iter().position(|s| *s == self.sort).unwrap_or(0);
        self.modal = Some(Modal::Picker { title: "Sort groups by".into(), items, cursor, kind: PickerKind::Sort });
    }

    fn open_views(&mut self) {
        let items = self.ctx.config.views.iter().map(|v| format!("{:<18} {}", v.name, v.description)).collect();
        self.modal =
            Some(Modal::Picker { title: "Views".into(), items, cursor: self.view_idx, kind: PickerKind::View });
    }

    fn open_keep_rules(&mut self) {
        let scope = self.current_group_path();
        let default = self.base_view().keep_rule.clone();
        let mut rules: Vec<&kinsift_core::config::KeepRuleConfig> = self.ctx.config.keep_rules.iter().collect();
        rules.sort_by_key(|r| Some(&r.name) != default.as_ref());
        let items = rules.iter().map(|r| format!("{:<18} {}", r.name, r.description)).collect();
        let where_ =
            if scope.is_empty() { "the whole view".to_owned() } else { format!("\"{}\"", self.node(&scope).label) };
        self.modal = Some(Modal::Picker {
            title: format!("Apply keep-rule to duplicate groups in {where_}"),
            items,
            cursor: 0,
            kind: PickerKind::KeepRule { scope },
        });
    }

    /// A background scan or hash works on its own snapshot of the files;
    /// changing files meanwhile would race with it.
    fn busy(&mut self) -> bool {
        let busy = self.scan.is_some();
        if busy {
            self.set_status("wait for the background job to finish before changing files");
        }
        busy
    }

    fn mark_at_cursor(&mut self) {
        match self.current().map(|r| r.kind.clone()) {
            Some(RowKind::File(f)) => {
                let id = self.entries[f].id;
                if !self.marked.remove(&id) {
                    self.marked.insert(id);
                }
            }
            Some(RowKind::Group(path)) => {
                let files = self.node(&path).all_files();
                let options = mark_options(&self.entries, &files);
                if options.is_empty() {
                    self.set_status("no field splits this group; mark files individually");
                    return;
                }
                let items = options
                    .iter()
                    .map(|(label, idx)| {
                        let marked = idx.iter().filter(|&&i| self.marked.contains(&self.entries[i].id)).count();
                        let state = if marked == idx.len() { " (all marked: unmark)" } else { "" };
                        format!("{label}  ({} files){state}", idx.len())
                    })
                    .collect();
                let title = format!("Mark files in \"{}\" where…", self.node(&path).label);
                self.modal = Some(Modal::Picker { title, items, cursor: 0, kind: PickerKind::MarkByValue { options } });
            }
            None => {}
        }
    }

    fn apply_keep_rule(&mut self, rule_name: &str, scope: &[usize]) {
        let Some(cfg) = self.ctx.config.keep_rule(rule_name).cloned() else { return };
        let rule = match KeepRule::from_config(&cfg) {
            Ok(r) => r,
            Err(e) => return self.set_status(format!("{e}")),
        };
        let groups: Vec<Vec<usize>> = self.node(scope).duplicate_groups().iter().map(|g| g.files.clone()).collect();
        let (mut n_files, mut n_groups) = (0, 0);
        for files in groups {
            let refs: Vec<&FileEntry> = files.iter().map(|&i| &self.entries[i]).collect();
            let chosen = rule.select(&refs);
            if !chosen.is_empty() {
                n_groups += 1;
            }
            for c in chosen {
                if self.marked.insert(refs[c].id) {
                    n_files += 1;
                }
            }
        }
        self.set_status(format!("{rule_name}: marked {n_files} files in {n_groups} groups (review with D)"));
    }

    fn open_rename(&mut self) {
        if self.busy() {
            return;
        }
        let Some(RowKind::File(f)) = self.current().map(|r| r.kind.clone()) else {
            return self.set_status("select a file to rename");
        };
        let name = self.entries[f].rel.rsplit('/').next().unwrap_or_default().to_owned();
        self.modal = Some(Modal::Input { title: "Rename to".into(), text: name, kind: InputKind::Rename(f) });
    }

    fn review_current_file(&mut self) {
        let Some(RowKind::File(f)) = self.current().map(|r| r.kind.clone()) else {
            return self.set_status("select a file, or mark files and press D");
        };
        self.open_review(vec![f]);
    }

    fn review_marked(&mut self) {
        let files: Vec<usize> =
            (0..self.entries.len()).filter(|&i| self.marked.contains(&self.entries[i].id)).collect();
        if files.is_empty() {
            return self.set_status("nothing marked (m marks files, K applies a keep-rule)");
        }
        self.open_review(files);
    }

    fn open_review(&mut self, files: Vec<usize>) {
        if self.busy() {
            return;
        }
        let mut groups: HashMap<String, Vec<FileEntry>> = HashMap::new();
        for &f in &files {
            if let Some(gid) = self.dup_of.get(&f)
                && !groups.contains_key(gid)
                && let Some(members) = self.dup_members.get(gid)
            {
                groups.insert(gid.clone(), members.iter().map(|&i| self.entries[i].clone()).collect());
            }
        }
        let selected = files.iter().map(|&i| self.entries[i].clone()).collect();
        match Plan::new(&self.ctx.index, &self.ctx.config, selected, groups) {
            Ok(plan) => self.modal = Some(Modal::Review(ReviewState { plan, cursor: 0 })),
            Err(e) => self.set_status(format!("cannot plan: {e}")),
        }
    }

    fn ask_undo(&mut self) {
        if self.busy() {
            return;
        }
        match self.ctx.index.last_undoable_batch() {
            Ok(Some(batch)) => {
                let count =
                    self.ctx.index.actions(Some(&batch)).map(|a| a.iter().filter(|a| !a.undone).count()).unwrap_or(0);
                self.modal = Some(Modal::ConfirmUndo { batch, count });
            }
            Ok(None) => self.set_status("nothing to undo"),
            Err(e) => self.set_status(format!("{e}")),
        }
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.take() else { return };
        let esc = key.code == KeyCode::Esc;
        match modal {
            Modal::Help | Modal::Message { .. } => {}
            Modal::Picker { title, items, mut cursor, kind } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = cursor.saturating_sub(1);
                    self.modal = Some(Modal::Picker { title, items, cursor, kind });
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    cursor = (cursor + 1).min(items.len().saturating_sub(1));
                    self.modal = Some(Modal::Picker { title, items, cursor, kind });
                }
                KeyCode::Enter => self.pick(kind, cursor, &items),
                _ if esc || key.code == KeyCode::Char('q') => {}
                _ => self.modal = Some(Modal::Picker { title, items, cursor, kind }),
            },
            Modal::Input { title, mut text, kind } => match key.code {
                KeyCode::Enter => self.submit_input(kind, text),
                KeyCode::Backspace => {
                    text.pop();
                    self.modal = Some(Modal::Input { title, text, kind });
                }
                KeyCode::Char(c) => {
                    text.push(c);
                    self.modal = Some(Modal::Input { title, text, kind });
                }
                _ if esc => {
                    if matches!(kind, InputKind::ConfirmPermanent) {
                        self.review_pending = None;
                    }
                }
                _ => self.modal = Some(Modal::Input { title, text, kind }),
            },
            Modal::Review(mut st) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    st.cursor = st.cursor.saturating_sub(1);
                    self.modal = Some(Modal::Review(st));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    st.cursor = (st.cursor + 1).min(st.plan.items.len().saturating_sub(1));
                    self.modal = Some(Modal::Review(st));
                }
                KeyCode::Char(' ') | KeyCode::Char('x') => {
                    st.plan.toggle(st.cursor);
                    self.modal = Some(Modal::Review(st));
                }
                KeyCode::Enter | KeyCode::Char('y') => {
                    if st.plan.totals().0 == 0 {
                        self.set_status("nothing to do");
                    } else if st.plan.needs_typed_confirmation() {
                        self.review_pending = Some(st.plan);
                        self.modal = Some(Modal::Input {
                            title: "PERMANENT delete. Type DELETE to confirm".into(),
                            text: String::new(),
                            kind: InputKind::ConfirmPermanent,
                        });
                    } else {
                        self.execute(st.plan);
                    }
                }
                _ if esc || key.code == KeyCode::Char('q') => self.set_status("cancelled; nothing changed"),
                _ => self.modal = Some(Modal::Review(st)),
            },
            Modal::ConfirmUndo { batch, count } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.undo(&batch),
                _ if esc || key.code == KeyCode::Char('n') => {}
                _ => self.modal = Some(Modal::ConfirmUndo { batch, count }),
            },
        }
    }

    fn pick(&mut self, kind: PickerKind, cursor: usize, items: &[String]) {
        if items.is_empty() {
            return;
        }
        match kind {
            PickerKind::Sort => {
                self.sort = SortBy::ALL[cursor];
                let key = self.cursor_key();
                sort_tree(&mut self.tree, self.sort, &self.entries);
                self.flatten_keeping(key);
                self.set_status(format!("sorted by {}", self.sort.label()));
            }
            PickerKind::View => {
                self.view_idx = cursor;
                self.group_override = None;
                self.filter_extra = None;
                self.dup_only = self.base_view().duplicates_only;
                self.expanded.clear();
                self.cursor = 0;
                if let Err(e) = self.reload() {
                    self.set_status(format!("{e}"));
                }
            }
            PickerKind::KeepRule { scope } => {
                let name = items[cursor].split_whitespace().next().unwrap_or_default().to_owned();
                self.apply_keep_rule(&name, &scope);
            }
            PickerKind::MarkByValue { options } => {
                let (label, idx) = &options[cursor];
                let all_marked = idx.iter().all(|&i| self.marked.contains(&self.entries[i].id));
                for &i in idx {
                    if all_marked {
                        self.marked.remove(&self.entries[i].id);
                    } else {
                        self.marked.insert(self.entries[i].id);
                    }
                }
                let verb = if all_marked { "unmarked" } else { "marked" };
                self.set_status(format!("{verb} {} files where {label}", idx.len()));
            }
        }
    }

    fn submit_input(&mut self, kind: InputKind, text: String) {
        let text = text.trim().to_owned();
        match kind {
            InputKind::Filter => {
                if !text.is_empty()
                    && let Err(e) = Filter::parse(&text)
                {
                    self.set_status(format!("{e}"));
                    self.modal = Some(Modal::Input { title: "Filter".into(), text, kind: InputKind::Filter });
                    return;
                }
                self.filter_extra = (!text.is_empty()).then_some(text);
                if let Err(e) = self.reload() {
                    self.set_status(format!("{e}"));
                }
            }
            InputKind::GroupBy => {
                let fields: Vec<String> =
                    text.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
                self.group_override = (!fields.is_empty()).then_some(fields);
                self.expanded.clear();
                self.rebuild();
            }
            InputKind::Rename(f) => self.rename(f, &text),
            InputKind::ConfirmPermanent => {
                if let Some(plan) = self.review_pending.take() {
                    if text == "DELETE" {
                        self.execute(plan);
                    } else {
                        self.set_status("not confirmed; nothing changed");
                    }
                }
            }
        }
    }

    fn execute(&mut self, mut plan: Plan) {
        if self.busy() {
            return;
        }
        let outcome = match actions::execute(&mut self.ctx.index, &mut plan) {
            Ok(o) => o,
            Err(e) => return self.set_status(format!("failed: {e}")),
        };
        let done: HashSet<FileId> = outcome.done.iter().map(|d| d.entry.id).collect();
        let bytes: u64 = outcome.done.iter().map(|d| d.entry.size).sum();
        self.marked.retain(|id| !done.contains(id));
        let key = self.cursor_key();
        self.entries.retain(|e| !done.contains(&e.id));
        self.rebuild_keeping(key);
        let how = match outcome.done.first().map(|d| d.op) {
            Some(StrategyKind::Trash) => "moved to the trash",
            Some(StrategyKind::Quarantine) => "moved to quarantine",
            Some(StrategyKind::Permanent) => "deleted permanently",
            None => "removed",
        };
        self.set_status(format!("{} files ({}) {how}; u to undo", outcome.done.len(), format_size(bytes)));
        if !outcome.skipped.is_empty() || !outcome.warnings.is_empty() {
            let mut lines = Vec::new();
            if !outcome.skipped.is_empty() {
                lines.push(format!("{} files were skipped:", outcome.skipped.len()));
                lines.extend(outcome.skipped.iter().map(|(p, r)| format!("{}: {r}", p.display())));
            }
            lines.extend(outcome.warnings.iter().cloned());
            self.modal = Some(Modal::Message { title: "Skipped".into(), lines });
        }
        self.refresh_quarantine_note();
    }

    fn rename(&mut self, f: usize, new_name: &str) {
        if self.busy() {
            return;
        }
        let entry = self.entries[f].clone();
        let policies = actions::root_policies(&self.ctx.index, &self.ctx.config).unwrap_or_default();
        let Some(policy) = policies.iter().find(|p| p.root == *entry.root) else { return };
        match actions::rename(&mut self.ctx.index, policy, &entry, new_name) {
            Ok(new_rel) => {
                let root = entry.root.as_path().to_path_buf();
                if let Err(e) =
                    refresh_files(&mut self.ctx.index, &self.ctx.providers, &self.ctx.priority, &root, &[new_rel])
                {
                    self.set_status(format!("renamed, but re-reading failed: {e}"));
                } else {
                    self.set_status(format!("renamed to {new_name}; u to undo"));
                }
                let _ = self.reload();
            }
            Err(e) => self.set_status(format!("rename failed: {e}")),
        }
    }

    fn undo(&mut self, batch: &str) {
        if self.busy() {
            return;
        }
        let outcome = match actions::undo_batch(&mut self.ctx.index, batch) {
            Ok(o) => o,
            Err(e) => return self.set_status(format!("undo failed: {e}")),
        };
        for (root, rel) in &outcome.removed {
            let _ = self.ctx.index.delete_by_rel(root, rel);
        }
        let mut by_root: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for (root, rel) in &outcome.restored {
            by_root.entry(root.clone()).or_default().push(rel.clone());
        }
        for (root, rels) in by_root {
            let _ = refresh_files(&mut self.ctx.index, &self.ctx.providers, &self.ctx.priority, &root, &rels);
        }
        let _ = self.reload();
        self.refresh_quarantine_note();
        if outcome.failed.is_empty() {
            self.set_status(format!("restored {} files", outcome.restored.len()));
        } else {
            let mut lines = vec![format!("restored {}, failed {}:", outcome.restored.len(), outcome.failed.len())];
            lines.extend(outcome.failed.iter().map(|(p, r)| format!("{}: {r}", p.display())));
            self.modal = Some(Modal::Message { title: "Undo".into(), lines });
        }
    }

    pub fn refresh_quarantine_note(&mut self) {
        let dirs: Vec<PathBuf> = actions::root_policies(&self.ctx.index, &self.ctx.config)
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.quarantine_dir)
            .collect();
        let batches = quarantine::list(&dirs).unwrap_or_default();
        let (size, oldest) = quarantine::usage(&batches);
        self.quarantine_note = (size > 0).then(|| {
            let limit = parse_size(&self.ctx.config.trash.remind_size).unwrap_or(u64::MAX);
            let days = oldest.unwrap_or(0) / 86_400;
            let over = size > limit || days > i64::from(self.ctx.config.trash.remind_age_days);
            let hint = if over { " — consider `kinsift trash purge`" } else { "" };
            format!("quarantine {} (oldest {days}d){hint}", format_size(size))
        });
    }

    /// Detail rows for the file at the cursor (cached).
    pub fn detail_sources(&mut self) -> Option<&[SourceRow]> {
        let id = self.current_file()?.id;
        if self.detail_cache.as_ref().map(|(i, _)| *i) != Some(id) {
            let rows = self.ctx.index.sources(id).unwrap_or_default();
            self.detail_cache = Some((id, rows));
        }
        self.detail_cache.as_ref().map(|(_, r)| r.as_slice())
    }

    /// Total size of marked files.
    pub fn marked_size(&self) -> u64 {
        self.entries.iter().filter(|e| self.marked.contains(&e.id)).map(|e| e.size).sum()
    }
}

/// Fields that split a group into a few subsets, for mark-by-value.
fn mark_options(entries: &[FileEntry], files: &[usize]) -> Vec<(String, Vec<usize>)> {
    const SKIP: &[&str] = &[
        "fs.path",
        "fs.name",
        "fs.size",
        "fs.mtime",
        "fs.root",
        "media.episode_title",
        "media.confidence",
        "content.hash",
        "content.partial_hash",
    ];
    let mut by_field: std::collections::BTreeMap<&str, std::collections::BTreeMap<String, (String, Vec<usize>)>> =
        Default::default();
    for &i in files {
        for (k, v) in &entries[i].fields {
            if SKIP.contains(&k.as_str()) || k.starts_with("meta.") {
                continue;
            }
            let slot = by_field
                .entry(k.as_str())
                .or_default()
                .entry(v.group_key())
                .or_insert_with(|| (display(k, v), Vec::new()));
            slot.1.push(i);
        }
    }
    let mut out = Vec::new();
    for (field, values) in by_field {
        if values.len() < 2 || values.len() > 12 {
            continue;
        }
        for (_, (label, idx)) in values {
            out.push((format!("{field} = {label}"), idx));
        }
    }
    out
}

fn display(key: &str, v: &Value) -> String {
    match (key, v) {
        ("video.resolution", Value::Int(r)) => format!("{r}p"),
        _ => v.to_string(),
    }
}
