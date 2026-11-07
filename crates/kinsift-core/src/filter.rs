//! View filters: field comparisons joined by `and`, compiled to SQL.
//!
//! ```text
//! name.type = episode and video.height >= 2160 and fs.size > 1GiB
//! name.title ~ "comet"              (case-insensitive contains)
//! exif.taken_at is not null
//! exif.taken_at >= 2024-01-01        (dates and "2024-01-01 12:30" in UTC)
//! ```

use std::collections::BTreeMap;

use rusqlite::types::Value as SqlValue;

use crate::config::parse_size;
use crate::error::{Error, Result};
use crate::value::{Value, unix_from_civil};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    IsNull,
    NotNull,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// A number, plus the word as typed: `name.title = 1917` must also
    /// match the text "1917".
    Num(f64, String),
    Text(String),
    /// A date (`2024-01-01`, `"2024-01-01 12:30"`): compared as a time
    /// against time fields, as typed against text fields.
    Date(i64, String),
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Clause {
    pub key: String,
    pub op: Op,
    pub value: Literal,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub clauses: Vec<Clause>,
    /// Restrict to one file id (internal use).
    id: Option<i64>,
}

impl Filter {
    pub fn parse(input: &str) -> Result<Filter> {
        let tokens = tokenize(input)?;
        let mut clauses = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let key = match &tokens[i] {
                Tok::Word(w) if w.contains('.') => w.clone(),
                t => return Err(Error::Filter(format!("expected a field like `video.height`, found `{}`", t.text()))),
            };
            i += 1;
            let op_tok = tokens.get(i).ok_or_else(|| Error::Filter(format!("missing operator after `{key}`")))?;
            i += 1;
            let (op, value) = match op_tok {
                Tok::Word(w) if w.eq_ignore_ascii_case("is") => {
                    let mut negate = false;
                    if matches!(tokens.get(i), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("not")) {
                        negate = true;
                        i += 1;
                    }
                    match tokens.get(i) {
                        Some(Tok::Word(w)) if w.eq_ignore_ascii_case("null") => i += 1,
                        _ => return Err(Error::Filter(format!("expected `null` after `{key} is`"))),
                    }
                    (if negate { Op::NotNull } else { Op::IsNull }, Literal::None)
                }
                Tok::Op(o) => {
                    let op = match o.as_str() {
                        "=" | "==" => Op::Eq,
                        "!=" => Op::Ne,
                        "<" => Op::Lt,
                        "<=" => Op::Le,
                        ">" => Op::Gt,
                        ">=" => Op::Ge,
                        "~" => Op::Contains,
                        other => return Err(Error::Filter(format!("unknown operator `{other}`"))),
                    };
                    let lit = match tokens.get(i) {
                        Some(Tok::Word(w)) if op == Op::Contains => Literal::Text(w.clone()),
                        Some(Tok::Word(w)) => parse_literal(w),
                        Some(Tok::Quoted(q)) if op != Op::Contains && parse_date(q).is_some() => parse_literal(q),
                        Some(Tok::Quoted(q)) => Literal::Text(q.clone()),
                        _ => return Err(Error::Filter(format!("missing value after `{key} {o}`"))),
                    };
                    i += 1;
                    (op, lit)
                }
                t => return Err(Error::Filter(format!("expected an operator after `{key}`, found `{}`", t.text()))),
            };
            clauses.push(Clause { key, op, value });
            match tokens.get(i) {
                None => break,
                Some(Tok::Word(w)) if w.eq_ignore_ascii_case("and") => i += 1,
                Some(t) => {
                    return Err(Error::Filter(format!(
                        "expected `and` between conditions, found `{}` (only `and` is supported)",
                        t.text()
                    )));
                }
            }
            if i >= tokens.len() {
                return Err(Error::Filter("filter ends with `and`".into()));
            }
        }
        Ok(Filter { clauses, id: None })
    }

    pub fn by_id(id: i64) -> Filter {
        Filter { clauses: Vec::new(), id: Some(id) }
    }

    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty() && self.id.is_none()
    }

    /// Combine two filters with `and`.
    pub fn and(mut self, other: Filter) -> Filter {
        self.clauses.extend(other.clauses);
        self.id = self.id.or(other.id);
        self
    }

    /// SQL condition over the `files` table plus its parameters.
    pub fn to_sql(&self) -> (String, Vec<SqlValue>) {
        let mut parts = Vec::new();
        let mut args = Vec::new();
        if let Some(id) = self.id {
            parts.push("files.id = ?".to_owned());
            args.push(SqlValue::Integer(id));
        }
        for c in &self.clauses {
            let exists = "EXISTS (SELECT 1 FROM fields f WHERE f.file_id = files.id AND f.key = ?";
            args.push(SqlValue::Text(c.key.clone()));
            let sql = match (&c.op, &c.value) {
                (Op::IsNull, _) => format!("NOT {exists})"),
                (Op::NotNull, _) => format!("{exists})"),
                (op @ (Op::Eq | Op::Ne), Literal::Num(n, raw)) => {
                    args.push(SqlValue::Real(*n));
                    args.push(SqlValue::Text(raw.to_lowercase()));
                    let not = if *op == Op::Ne { "NOT " } else { "" };
                    format!("{not}{exists} AND (COALESCE(f.int_val, f.real_val) = ? OR lower(f.text_val) = ?))")
                }
                (op, Literal::Num(n, _)) => {
                    args.push(SqlValue::Real(*n));
                    format!("{exists} AND COALESCE(f.int_val, f.real_val) {} ?)", sql_op(*op))
                }
                (op, Literal::Date(t, raw)) => {
                    args.push(SqlValue::Integer(*t));
                    args.push(SqlValue::Text(raw.to_lowercase()));
                    let (not, op) = if *op == Op::Ne { ("NOT ", "=") } else { ("", sql_op(*op)) };
                    format!(
                        "{not}{exists} AND CASE WHEN f.text_val IS NULL \
                         THEN COALESCE(f.int_val, f.real_val) {op} ? ELSE lower(f.text_val) {op} ? END)"
                    )
                }
                (Op::Contains, Literal::Text(t)) => {
                    args.push(SqlValue::Text(t.to_lowercase()));
                    format!("{exists} AND instr(lower(f.text_val), ?) > 0)")
                }
                (Op::Ne, Literal::Text(t)) => {
                    args.push(SqlValue::Text(t.to_lowercase()));
                    format!("NOT {exists} AND lower(f.text_val) = ?)")
                }
                (op, Literal::Text(t)) => {
                    args.push(SqlValue::Text(t.to_lowercase()));
                    format!("{exists} AND lower(f.text_val) {} ?)", sql_op(*op))
                }
                (_, Literal::None) => format!("{exists})"),
            };
            parts.push(sql);
        }
        if parts.is_empty() { ("1=1".to_owned(), args) } else { (parts.join(" AND "), args) }
    }

    /// Evaluate the filter in memory (same semantics as the SQL).
    pub fn matches(&self, fields: &BTreeMap<String, Value>) -> bool {
        self.clauses.iter().all(|c| {
            let v = fields.get(&c.key);
            match (&c.op, &c.value, v) {
                (Op::IsNull, _, v) => v.is_none(),
                (Op::NotNull, _, v) => v.is_some(),
                (Op::Ne, lit, v) => !v.is_some_and(|v| lit_eq(lit, v)),
                (_, _, None) => false,
                (Op::Eq, lit @ Literal::Num(..), Some(v)) => lit_eq(lit, v),
                (op, Literal::Num(n, _), Some(v)) => v.as_f64().is_some_and(|x| cmp_num(*op, x, *n)),
                (op, Literal::Date(t, raw), Some(v)) => match v {
                    Value::Str(_) | Value::Path(_) | Value::List(_) => cmp_text(*op, &text_of(v), &raw.to_lowercase()),
                    _ => v.as_f64().is_some_and(|x| cmp_num(*op, x, *t as f64)),
                },
                (Op::Contains, Literal::Text(t), Some(v)) => text_of(v).contains(&t.to_lowercase()),
                (op, Literal::Text(t), Some(v)) => cmp_text(*op, &text_of(v), &t.to_lowercase()),
                (_, Literal::None, Some(_)) => true,
            }
        })
    }
}

fn sql_op(op: Op) -> &'static str {
    match op {
        Op::Eq => "=",
        Op::Ne => "!=",
        Op::Lt => "<",
        Op::Le => "<=",
        Op::Gt => ">",
        Op::Ge => ">=",
        Op::Contains | Op::IsNull | Op::NotNull => "=",
    }
}

/// Text as stored in `text_val` (lists are JSON there), lower-cased.
fn text_of(v: &Value) -> String {
    match v {
        Value::List(_) => v.to_sql().3.unwrap_or_default().to_lowercase(),
        Value::Str(s) => s.to_lowercase(),
        Value::Path(p) => p.join("/").to_lowercase(),
        _ => String::new(),
    }
}

fn lit_eq(lit: &Literal, v: &Value) -> bool {
    match lit {
        Literal::Num(n, raw) => v.as_f64() == Some(*n) || text_of(v) == raw.to_lowercase(),
        Literal::Text(t) => text_of(v) == t.to_lowercase(),
        Literal::Date(t, raw) => match v {
            Value::Str(_) | Value::Path(_) | Value::List(_) => text_of(v) == raw.to_lowercase(),
            _ => v.as_i64() == Some(*t),
        },
        Literal::None => true,
    }
}

fn cmp_num(op: Op, x: f64, n: f64) -> bool {
    match op {
        Op::Eq => x == n,
        Op::Lt => x < n,
        Op::Le => x <= n,
        Op::Gt => x > n,
        Op::Ge => x >= n,
        _ => false,
    }
}

fn cmp_text(op: Op, s: &str, t: &str) -> bool {
    match op {
        Op::Eq => s == t,
        Op::Lt => s < t,
        Op::Le => s <= t,
        Op::Gt => s > t,
        Op::Ge => s >= t,
        _ => false,
    }
}

/// `YYYY-MM-DD`, optionally followed by ` HH:MM[:SS]` or `THH:MM[:SS]` (UTC).
fn parse_date(w: &str) -> Option<i64> {
    let (date, time) = match w.split_once([' ', 'T']) {
        Some((d, t)) => (d, Some(t)),
        None => (w, None),
    };
    // A fixed number of digits; parses as u32 (the year fits).
    let num = |s: &str, len: usize| -> Option<u32> {
        (s.len() == len && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
    };
    let mut d = date.split('-');
    let (y, m, day) = (num(d.next()?, 4)?, num(d.next()?, 2)?, num(d.next()?, 2)?);
    if d.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut hh, mut mm, mut ss) = (0, 0, 0);
    if let Some(time) = time {
        let mut t = time.split(':');
        hh = num(t.next()?, 2)?;
        mm = num(t.next()?, 2)?;
        if let Some(s) = t.next() {
            ss = num(s, 2)?;
        }
        if t.next().is_some() || hh > 23 || mm > 59 || ss > 59 {
            return None;
        }
    }
    Some(unix_from_civil(i64::from(y), m, day, hh, mm, ss))
}

fn parse_literal(w: &str) -> Literal {
    if let Some(t) = parse_date(w) {
        return Literal::Date(t, w.to_owned());
    }
    if let Ok(n) = w.parse::<f64>() {
        return Literal::Num(n, w.to_owned());
    }
    if w.starts_with(|c: char| c.is_ascii_digit())
        && let Some(bytes) = parse_size(w)
    {
        return Literal::Num(bytes as f64, w.to_owned());
    }
    Literal::Text(w.to_owned())
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Quoted(String),
    Op(String),
}

impl Tok {
    fn text(&self) -> &str {
        match self {
            Tok::Word(s) | Tok::Quoted(s) | Tok::Op(s) => s,
        }
    }
}

fn tokenize(input: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' || c == '\'' {
            chars.next();
            let mut s = String::new();
            loop {
                match chars.next() {
                    Some(ch) if ch == c => break,
                    Some(ch) => s.push(ch),
                    None => return Err(Error::Filter("unterminated quote".into())),
                }
            }
            out.push(Tok::Quoted(s));
        } else if "=!<>~".contains(c) {
            let mut s = String::new();
            while let Some(&ch) = chars.peek() {
                if "=!<>~".contains(ch) {
                    s.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(Tok::Op(s));
        } else {
            let mut s = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() || "=!<>~\"'".contains(ch) {
                    break;
                }
                s.push(ch);
                chars.next();
            }
            out.push(Tok::Word(s));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn parses_and_matches() {
        let f = Filter::parse("name.type = episode and video.height >= 2160 and fs.size > 1GiB").unwrap();
        assert_eq!(f.clauses.len(), 3);
        let yes = fields(&[
            ("name.type", Value::Str("Episode".into())),
            ("video.height", Value::Int(2160)),
            ("fs.size", Value::Int(2 << 30)),
        ]);
        assert!(f.matches(&yes));
        let no = fields(&[("name.type", Value::Str("episode".into())), ("video.height", Value::Int(1080))]);
        assert!(!f.matches(&no));
    }

    #[test]
    fn null_checks_and_contains() {
        let f = Filter::parse("exif.taken_at is not null and name.title ~ \"comet riders\" and x.y is null").unwrap();
        let v = fields(&[("exif.taken_at", Value::Time(1)), ("name.title", Value::Str("Comet Riders Rising".into()))]);
        assert!(f.matches(&v));
    }

    #[test]
    fn errors_are_helpful() {
        assert!(Filter::parse("height > 3").unwrap_err().to_string().contains("expected a field"));
        assert!(Filter::parse("a.b > 3 or c.d < 2").unwrap_err().to_string().contains("only `and`"));
        assert!(Filter::parse("a.b >").is_err());
        assert!(Filter::parse("a.b = 1 and").is_err());
    }

    #[test]
    fn not_equal_includes_missing() {
        let f = Filter::parse("name.type != movie").unwrap();
        assert!(f.matches(&fields(&[])));
        assert!(!f.matches(&fields(&[("name.type", Value::Str("movie".into()))])));
    }

    #[test]
    fn dates_compare_against_times_and_text() {
        let t = |y, m, d| Value::Time(unix_from_civil(y, m, d, 0, 0, 0));
        let f = Filter::parse("exif.taken_at >= 2024-01-01 and exif.taken_at < \"2024-06-30 12:00\"").unwrap();
        assert!(f.matches(&fields(&[("exif.taken_at", t(2024, 3, 1))])));
        assert!(!f.matches(&fields(&[("exif.taken_at", t(2023, 12, 31))])));
        assert!(!f.matches(&fields(&[("exif.taken_at", t(2024, 7, 1))])));
        // Text fields still compare as typed.
        let d = Filter::parse("media.date > 2024-01-01").unwrap();
        assert!(d.matches(&fields(&[("media.date", Value::Str("2024-02-03".into()))])));
        assert!(
            Filter::parse("media.date = 2024-13-01").unwrap().clauses[0].value == Literal::Text("2024-13-01".into())
        );

        let mut index = crate::index::Index::open_in_memory().unwrap();
        let root = index.ensure_root(std::path::Path::new("/r")).unwrap();
        let ids = index.upsert_files(root.id, &[("a.jpg".into(), 1, 1), ("b.jpg".into(), 1, 1)]).unwrap();
        let set = |id, v: Value| crate::index::ExtractionWrite {
            file_id: id,
            provider: "exif",
            version: 1,
            outcome: Ok(Some(crate::provider::Extraction {
                sets: vec![crate::domain::FieldSet { domain: "exif", fields: vec![("taken_at", v)] }],
                raw: None,
            })),
        };
        index
            .write_extractions(vec![set(ids[0], t(2024, 3, 1)), set(ids[1], t(2023, 1, 1))], &Default::default())
            .unwrap();
        let hits = index.load_entries(Some(&f), None).unwrap();
        assert_eq!(hits.iter().map(|e| e.rel.as_str()).collect::<Vec<_>>(), ["a.jpg"]);
    }

    #[test]
    fn numbers_also_match_text_values() {
        let f = Filter::parse("name.title = 1917").unwrap();
        assert!(f.matches(&fields(&[("name.title", Value::Str("1917".into()))])));
        assert!(
            Filter::parse("video.resolution = 1080")
                .unwrap()
                .matches(&fields(&[("video.resolution", Value::Int(1080))]))
        );
        let ne = Filter::parse("name.title != 1917").unwrap();
        assert!(!ne.matches(&fields(&[("name.title", Value::Str("1917".into()))])));

        let mut index = crate::index::Index::open_in_memory().unwrap();
        let root = index.ensure_root(std::path::Path::new("/r")).unwrap();
        let ids = index.upsert_files(root.id, &[("a.mkv".into(), 1, 1), ("b.mkv".into(), 1, 1)]).unwrap();
        let set = |id, title: &str| crate::index::ExtractionWrite {
            file_id: id,
            provider: "name",
            version: 1,
            outcome: Ok(Some(crate::provider::Extraction {
                sets: vec![crate::domain::FieldSet {
                    domain: "name",
                    fields: vec![("title", Value::Str(title.into()))],
                }],
                raw: None,
            })),
        };
        index.write_extractions(vec![set(ids[0], "1917"), set(ids[1], "Other")], &Default::default()).unwrap();
        let hits = index.load_entries(Some(&f), None).unwrap();
        assert_eq!(hits.iter().map(|e| e.rel.as_str()).collect::<Vec<_>>(), ["a.mkv"]);
        assert_eq!(index.load_entries(Some(&ne), None).unwrap().len(), 1);
    }
}
