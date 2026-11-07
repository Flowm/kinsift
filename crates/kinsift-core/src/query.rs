//! Turning files into a tree of groups according to a view.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::domain::field_info;
use crate::index::FileEntry;
use crate::value::{Value, format_unix, natural_cmp};

/// Label of groups for files without a value.
pub const NONE_LABEL: &str = "(none)";
const NONE_KEY: &str = "\u{0}none";

/// A group in the tree.
#[derive(Debug, Clone, Default)]
pub struct Node {
    pub label: String,
    pub key: String,
    /// Unique id of this node within the tree (keys joined from the root).
    pub id: String,
    /// The group-by field this node groups by (empty for the root).
    pub field: String,
    pub children: Vec<Node>,
    /// Indices into the entry slice of files directly in this group.
    pub files: Vec<usize>,
    pub size: u64,
    pub file_count: usize,
    /// This node is a last-level group with more than one file.
    pub duplicate: bool,
    /// Number of duplicate groups in this subtree.
    pub dup_groups: usize,
}

impl Node {
    pub fn is_none_group(&self) -> bool {
        self.key == NONE_KEY
    }

    /// All file indices in this subtree.
    pub fn all_files(&self) -> Vec<usize> {
        let mut out = self.files.clone();
        for c in &self.children {
            out.extend(c.all_files());
        }
        out
    }

    /// All duplicate groups (nodes) in this subtree.
    pub fn duplicate_groups(&self) -> Vec<&Node> {
        let mut out = Vec::new();
        if self.duplicate {
            out.push(self);
        }
        for c in &self.children {
            out.extend(c.duplicate_groups());
        }
        out
    }

    pub fn find(&self, id: &str) -> Option<&Node> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    Name,
    Size,
    Count,
    Duplicates,
}

impl SortBy {
    pub const ALL: [SortBy; 4] = [SortBy::Name, SortBy::Size, SortBy::Count, SortBy::Duplicates];

    pub fn label(self) -> &'static str {
        match self {
            SortBy::Name => "name",
            SortBy::Size => "total size",
            SortBy::Count => "file count",
            SortBy::Duplicates => "duplicate count",
        }
    }
}

struct Step {
    key: String,
    label: String,
    field: String,
    last_level: bool,
}

/// A group-by spec: `field` or `field:modifier`.
#[derive(Debug, Clone)]
pub struct GroupSpec {
    pub field: String,
    pub modifier: Option<String>,
}

impl GroupSpec {
    pub fn parse(spec: &str) -> GroupSpec {
        match spec.split_once(':') {
            Some((f, m)) => GroupSpec { field: f.to_owned(), modifier: Some(m.to_owned()) },
            None => GroupSpec { field: spec.to_owned(), modifier: None },
        }
    }
}

fn steps_for(entry: &FileEntry, specs: &[GroupSpec]) -> Vec<Step> {
    let mut steps = Vec::new();
    for (li, spec) in specs.iter().enumerate() {
        let last_level = li + 1 == specs.len();
        match entry.get(&spec.field) {
            None => steps.push(Step {
                key: NONE_KEY.into(),
                label: NONE_LABEL.into(),
                field: spec.field.clone(),
                last_level,
            }),
            Some(Value::Path(segments)) => {
                let dirs = &segments[..segments.len().saturating_sub(1)];
                for seg in dirs {
                    steps.push(Step {
                        key: seg.clone(),
                        label: seg.clone(),
                        field: spec.field.clone(),
                        last_level: false,
                    });
                }
            }
            Some(Value::Time(t)) if spec.modifier.is_some() => {
                let full = format_unix(*t);
                let label = match spec.modifier.as_deref() {
                    Some("year") => full[..4].to_owned(),
                    Some("month") => full[..7].to_owned(),
                    _ => full[..10].to_owned(),
                };
                steps.push(Step { key: label.clone(), label, field: spec.field.clone(), last_level });
            }
            Some(v) => steps.push(Step {
                key: v.group_key(),
                label: format_label(&spec.field, v),
                field: spec.field.clone(),
                last_level,
            }),
        }
    }
    steps
}

/// Display label for a group value, e.g. "Season 3".
pub fn format_label(field: &str, v: &Value) -> String {
    match field_info(field) {
        Some(info) if !info.label.is_empty() && v.as_f64().is_some() => format!("{}{}", info.label, v),
        _ => v.to_string(),
    }
}

#[derive(Default)]
struct Builder {
    key: String,
    field: String,
    labels: HashMap<String, usize>,
    children: Vec<Builder>,
    index: HashMap<String, usize>,
    files: Vec<usize>,
    last_level: bool,
}

impl Builder {
    fn finish(self, parent_id: &str, entries: &[FileEntry], duplicates_only: bool) -> Option<Node> {
        let id = if parent_id.is_empty() { self.key.clone() } else { format!("{parent_id}\u{1f}{}", self.key) };
        let label = self
            .labels
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            .map(|(l, _)| l)
            .unwrap_or_default();
        let duplicate = self.last_level && self.files.len() > 1;
        let mut children: Vec<Node> =
            self.children.into_iter().filter_map(|c| c.finish(&id, entries, duplicates_only)).collect();
        let mut files = self.files;
        if duplicates_only && !duplicate {
            files.clear();
        }
        children.retain(|c| !duplicates_only || c.file_count > 0);
        let size = files.iter().map(|&i| entries[i].size).sum::<u64>() + children.iter().map(|c| c.size).sum::<u64>();
        let file_count = files.len() + children.iter().map(|c| c.file_count).sum::<usize>();
        let dup_groups = usize::from(duplicate) + children.iter().map(|c| c.dup_groups).sum::<usize>();
        Some(Node {
            label,
            key: self.key,
            id,
            field: self.field,
            children,
            files,
            size,
            file_count,
            duplicate,
            dup_groups,
        })
    }
}

/// Group `entries` by the `group_by` specs into a tree.
pub fn build_tree(entries: &[FileEntry], group_by: &[String], duplicates_only: bool) -> Node {
    let specs: Vec<GroupSpec> = group_by.iter().map(|s| GroupSpec::parse(s)).collect();
    let mut root = Builder::default();
    for (i, entry) in entries.iter().enumerate() {
        let mut node = &mut root;
        for step in steps_for(entry, &specs) {
            let pos = match node.index.get(&step.key) {
                Some(&p) => p,
                None => {
                    node.children.push(Builder {
                        key: step.key.clone(),
                        field: step.field.clone(),
                        last_level: step.last_level,
                        ..Default::default()
                    });
                    node.index.insert(step.key.clone(), node.children.len() - 1);
                    node.children.len() - 1
                }
            };
            node = &mut node.children[pos];
            *node.labels.entry(step.label).or_default() += 1;
        }
        node.files.push(i);
    }
    let mut tree = root.finish("", entries, duplicates_only).expect("root node");
    sort_tree(&mut tree, SortBy::Name, entries);
    tree
}

/// Sort children (recursively) and the files within each group.
pub fn sort_tree(node: &mut Node, by: SortBy, entries: &[FileEntry]) {
    node.children.sort_by(|a, b| {
        // "(none)" always goes last.
        a.is_none_group().cmp(&b.is_none_group()).then_with(|| {
            let ord = match by {
                SortBy::Name => Ordering::Equal,
                SortBy::Size => b.size.cmp(&a.size),
                SortBy::Count => b.file_count.cmp(&a.file_count),
                SortBy::Duplicates => b.dup_groups.cmp(&a.dup_groups),
            };
            ord.then_with(|| natural_cmp(&a.label, &b.label))
        })
    });
    node.files.sort_by(|&a, &b| natural_cmp(&entries[a].rel, &entries[b].rel));
    for c in &mut node.children {
        sort_tree(c, by, entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn entry(rel: &str, size: u64, fields: &[(&str, Value)]) -> FileEntry {
        FileEntry {
            id: 0,
            root: Arc::new(PathBuf::from("/r")),
            rel: rel.into(),
            size,
            mtime_ns: 0,
            fields: fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<BTreeMap<_, _>>(),
        }
    }

    fn ep(rel: &str, title: &str, s: i64, e: i64) -> FileEntry {
        entry(
            rel,
            100,
            &[
                ("name.title", Value::Str(title.into())),
                ("name.season", Value::Int(s)),
                ("name.episode", Value::Int(e)),
            ],
        )
    }

    fn by() -> Vec<String> {
        vec!["name.title".into(), "name.season".into(), "name.episode".into()]
    }

    #[test]
    fn groups_and_finds_duplicates() {
        let entries = vec![
            ep("a", "Ferris's Agents of P.R.I.S.M.", 1, 1),
            ep("b", "Ferriss Agents of PRISM", 1, 1),
            ep("c", "Ferriss Agents of PRISM", 1, 2),
        ];
        let tree = build_tree(&entries, &by(), false);
        assert_eq!(tree.children.len(), 1, "title variants group together");
        let show = &tree.children[0];
        assert_eq!(show.label, "Ferriss Agents of PRISM", "most common spelling wins");
        assert_eq!(show.children[0].label, "Season 1");
        assert_eq!(tree.dup_groups, 1);
        assert_eq!(tree.file_count, 3);

        let dups = build_tree(&entries, &by(), true);
        assert_eq!(dups.file_count, 2);
        assert_eq!(dups.duplicate_groups().len(), 1);
    }

    #[test]
    fn path_values_expand_into_folders() {
        let p = |s: &str| Value::Path(s.split('/').map(str::to_owned).collect());
        let entries = vec![
            entry("Show/S1/a.mkv", 1, &[("fs.path", p("Show/S1/a.mkv"))]),
            entry("Show/S1/b.mkv", 2, &[("fs.path", p("Show/S1/b.mkv"))]),
            entry("top.mkv", 4, &[("fs.path", p("top.mkv"))]),
        ];
        let tree = build_tree(&entries, &["fs.path".into()], false);
        assert_eq!(tree.files.len(), 1, "top-level file sits at the root");
        assert_eq!(tree.children[0].label, "Show");
        assert_eq!(tree.children[0].children[0].label, "S1");
        assert_eq!(tree.children[0].size, 3);
        assert_eq!(tree.dup_groups, 0, "folders are never duplicate groups");
    }

    #[test]
    fn missing_values_and_time_buckets() {
        let entries = vec![
            entry("x", 1, &[("exif.taken_at", Value::Time(crate::value::unix_from_civil(2023, 7, 4, 0, 0, 0)))]),
            entry("y", 1, &[]),
        ];
        let tree = build_tree(&entries, &["exif.taken_at:year".into(), "exif.taken_at:month".into()], false);
        assert_eq!(tree.children[0].label, "2023");
        assert_eq!(tree.children[0].children[0].label, "2023-07");
        assert_eq!(tree.children[1].label, NONE_LABEL);
    }

    #[test]
    fn sorting() {
        let entries = vec![ep("a", "B", 1, 1), ep("b", "A", 1, 1), ep("c", "B", 1, 1)];
        let mut tree = build_tree(&entries, &by(), false);
        assert_eq!(tree.children[0].label, "A");
        sort_tree(&mut tree, SortBy::Count, &entries);
        assert_eq!(tree.children[0].label, "B");
    }
}
