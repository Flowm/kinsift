//! Keep-rules: rank the files of a duplicate group and decide which to mark.

use std::cmp::Ordering;

use crate::config::KeepRuleConfig;
use crate::error::{Error, Result};
use crate::index::FileEntry;
use crate::normalize::title_key;
use crate::value::{Value, natural_cmp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Keep exactly one file per group; mark the rest.
    KeepBest,
    /// Mark only files strictly worse than the best on the first term.
    DropDominated,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    /// Higher is better.
    Desc(String),
    /// Lower is better.
    Asc(String),
    /// Earlier in the list is better; values not in the list rank last.
    In(String, Vec<String>),
    /// Files without the field rank first.
    IsNull(String),
    /// Files with the field rank first.
    NotNull(String),
}

impl Term {
    pub fn field(&self) -> &str {
        match self {
            Term::Desc(f) | Term::Asc(f) | Term::In(f, _) | Term::IsNull(f) | Term::NotNull(f) => f,
        }
    }

    pub fn parse(s: &str) -> Result<Term> {
        let s = s.trim();
        let (field, rest) = s.split_once(char::is_whitespace).unwrap_or((s, ""));
        let rest = rest.trim();
        let field = field.to_owned();
        let lower = rest.to_ascii_lowercase();
        Ok(match lower.as_str() {
            "" | "desc" => Term::Desc(field),
            "asc" => Term::Asc(field),
            "is null" => Term::IsNull(field),
            "is not null" => Term::NotNull(field),
            _ if lower.starts_with("in ") => {
                let list = rest[3..].trim();
                let inner = list
                    .strip_prefix('[')
                    .and_then(|l| l.strip_suffix(']'))
                    .ok_or_else(|| Error::Config(format!("keep-rule term `{s}`: expected `in [a, b]`")))?;
                Term::In(field, inner.split(',').map(|v| title_key(v.trim())).filter(|v| !v.is_empty()).collect())
            }
            _ => return Err(Error::Config(format!("keep-rule term `{s}`: expected desc, asc, in [..], is null"))),
        })
    }

    /// Compare two files on this term; `Greater` means `a` is better.
    fn compare(&self, a: &FileEntry, b: &FileEntry) -> Ordering {
        let (va, vb) = (a.get(self.field()), b.get(self.field()));
        match self {
            Term::Desc(_) => cmp_present(va, vb, |x, y| x.compare(y)),
            Term::Asc(_) => cmp_present(va, vb, |x, y| y.compare(x)),
            Term::In(_, list) => {
                let rank = |v: Option<&Value>| {
                    v.and_then(|v| list.iter().position(|item| *item == v.group_key())).map_or(0, |p| list.len() - p)
                };
                rank(va).cmp(&rank(vb))
            }
            Term::IsNull(_) => va.is_none().cmp(&vb.is_none()),
            Term::NotNull(_) => va.is_some().cmp(&vb.is_some()),
        }
    }
}

/// Missing values rank below present ones.
fn cmp_present(a: Option<&Value>, b: Option<&Value>, f: impl Fn(&Value, &Value) -> Ordering) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => f(x, y),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

#[derive(Debug, Clone)]
pub struct KeepRule {
    pub name: String,
    pub description: String,
    pub mode: Mode,
    pub terms: Vec<Term>,
}

impl KeepRule {
    pub fn from_config(c: &KeepRuleConfig) -> Result<KeepRule> {
        let mode = match c.mode.as_str() {
            "keep-best" => Mode::KeepBest,
            "drop-dominated" => Mode::DropDominated,
            other => return Err(Error::Config(format!("keep-rule `{}`: unknown mode `{other}`", c.name))),
        };
        let terms = c.rank.iter().map(|t| Term::parse(t)).collect::<Result<Vec<_>>>()?;
        if terms.is_empty() {
            return Err(Error::Config(format!("keep-rule `{}` has no rank terms", c.name)));
        }
        Ok(KeepRule { name: c.name.clone(), description: c.description.clone(), mode, terms })
    }

    fn compare(&self, a: &FileEntry, b: &FileEntry) -> Ordering {
        self.terms
            .iter()
            .map(|t| t.compare(a, b))
            .find(|o| *o != Ordering::Equal)
            // Deterministic tie-break: the alphabetically first path is "better".
            .unwrap_or_else(|| natural_cmp(&b.rel, &a.rel))
    }

    /// Indices (into `group`) of the files this rule marks for removal.
    ///
    /// `drop-dominated` never marks a file whose first-term field is missing:
    /// "unknown" is not "worse".
    pub fn select(&self, group: &[&FileEntry]) -> Vec<usize> {
        if group.len() < 2 {
            return Vec::new();
        }
        match self.mode {
            Mode::KeepBest => {
                let best = (0..group.len()).max_by(|&a, &b| self.compare(group[a], group[b])).expect("non-empty");
                (0..group.len()).filter(|&i| i != best).collect()
            }
            Mode::DropDominated => {
                let first = &self.terms[0];
                let known: Vec<usize> = (0..group.len()).filter(|&i| group[i].get(first.field()).is_some()).collect();
                let Some(&best) = known.iter().max_by(|&&a, &&b| first.compare(group[a], group[b])) else {
                    return Vec::new();
                };
                known.into_iter().filter(|&i| first.compare(group[i], group[best]) == Ordering::Less).collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn f(rel: &str, fields: &[(&str, Value)]) -> FileEntry {
        FileEntry {
            id: 0,
            root: Arc::new(PathBuf::from("/r")),
            rel: rel.into(),
            size: 0,
            mtime_ns: 0,
            fields: fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<BTreeMap<_, _>>(),
        }
    }

    fn rule(mode: &str, rank: &[&str]) -> KeepRule {
        KeepRule::from_config(&KeepRuleConfig {
            name: "t".into(),
            description: String::new(),
            mode: mode.into(),
            rank: rank.iter().map(|s| s.to_string()).collect(),
        })
        .unwrap()
    }

    fn res(r: i64) -> (&'static str, Value) {
        ("video.resolution", Value::Int(r))
    }

    #[test]
    fn drop_dominated_keeps_ties_and_unknowns() {
        let files =
            [f("a", &[res(2160)]), f("b", &[res(1080)]), f("c", &[res(720)]), f("d", &[res(2160)]), f("e", &[])];
        let refs: Vec<&FileEntry> = files.iter().collect();
        let marked = rule("drop-dominated", &["video.resolution desc"]).select(&refs);
        assert_eq!(marked, [1, 2], "both 2160p files and the unknown one are kept");
    }

    #[test]
    fn keep_best_uses_later_terms_for_ties() {
        let files = [
            f("a", &[res(1080), ("video.codec", Value::Str("h264".into())), ("fs.size", Value::Int(5))]),
            f("b", &[res(1080), ("video.codec", Value::Str("HEVC".into())), ("fs.size", Value::Int(9))]),
            f("c", &[res(720), ("video.codec", Value::Str("hevc".into())), ("fs.size", Value::Int(1))]),
        ];
        let refs: Vec<&FileEntry> = files.iter().collect();
        let r = rule("keep-best", &["video.resolution desc", "video.codec in [hevc, av1, h264]", "fs.size asc"]);
        assert_eq!(r.select(&refs), [0, 2], "1080p hevc wins");
    }

    #[test]
    fn null_terms_and_deterministic_ties() {
        let files = [f("b", &[("exif.software", Value::Str("Editor".into()))]), f("a", &[]), f("c", &[])];
        let refs: Vec<&FileEntry> = files.iter().collect();
        assert_eq!(rule("keep-best", &["exif.software is null"]).select(&refs), [0, 2], "unedited, then first path");
    }

    #[test]
    fn bad_terms_are_rejected() {
        assert!(Term::parse("x.y sideways").is_err());
        assert!(Term::parse("x.y in hevc").is_err());
        assert_eq!(Term::parse("x.y").unwrap(), Term::Desc("x.y".into()));
    }
}
