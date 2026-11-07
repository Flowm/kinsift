//! Typed metadata values and their SQLite representation.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::normalize::title_key;

/// A single typed metadata value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    /// Seconds since the Unix epoch (UTC).
    Time(i64),
    /// Seconds.
    Duration(f64),
    /// Hierarchical path, one element per segment (directories, then file name).
    Path(Vec<String>),
    List(Vec<Value>),
}

/// The columns a value is stored in: `(kind, int_val, real_val, text_val)`.
pub type SqlParts = (&'static str, Option<i64>, Option<f64>, Option<String>);

impl Value {
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Str(_) => "s",
            Value::Int(_) => "i",
            Value::Float(_) => "f",
            Value::Time(_) => "t",
            Value::Duration(_) => "d",
            Value::Path(_) => "p",
            Value::List(_) => "l",
        }
    }

    pub fn to_sql(&self) -> SqlParts {
        match self {
            Value::Str(s) => ("s", None, None, Some(s.clone())),
            Value::Int(i) => ("i", Some(*i), None, None),
            Value::Float(f) => ("f", None, Some(*f), None),
            Value::Time(t) => ("t", Some(*t), None, None),
            Value::Duration(d) => ("d", None, Some(*d), None),
            Value::Path(p) => ("p", None, None, Some(p.join("/"))),
            Value::List(items) => {
                let first_int = items.iter().find_map(Value::as_i64);
                let json = serde_json::to_string(items).unwrap_or_default();
                ("l", first_int, None, Some(json))
            }
        }
    }

    pub fn from_sql(
        kind: &str,
        int_val: Option<i64>,
        real_val: Option<f64>,
        text_val: Option<String>,
    ) -> Option<Value> {
        Some(match kind {
            "s" => Value::Str(text_val?),
            "i" => Value::Int(int_val?),
            "f" => Value::Float(real_val?),
            "t" => Value::Time(int_val?),
            "d" => Value::Duration(real_val?),
            "p" => Value::Path(text_val?.split('/').map(str::to_owned).collect()),
            "l" => Value::List(serde_json::from_str(&text_val?).ok()?),
            _ => return None,
        })
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) | Value::Time(i) => Some(*i),
            Value::Float(f) | Value::Duration(f) => Some(*f as i64),
            Value::List(items) => items.first().and_then(Value::as_i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) | Value::Time(i) => Some(*i as f64),
            Value::Float(f) | Value::Duration(f) => Some(*f),
            Value::List(items) => items.first().and_then(Value::as_f64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Key used to decide whether two values belong to the same group.
    /// Strings are normalized so that "Ferris's P.R.I.S.M." and
    /// "Ferriss PRISM" group together.
    pub fn group_key(&self) -> String {
        match self {
            Value::Str(s) => title_key(s),
            Value::Float(f) | Value::Duration(f) => format!("{f}"),
            Value::Int(i) | Value::Time(i) => i.to_string(),
            Value::Path(p) => p.join("/"),
            Value::List(items) => items.iter().map(Value::group_key).collect::<Vec<_>>().join(","),
        }
    }

    /// Ordering used for sorting and ranking: numbers numerically, text naturally.
    pub fn compare(&self, other: &Value) -> Ordering {
        match (self.as_f64(), other.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
            _ => natural_cmp(&self.to_string(), &other.to_string()),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Str(s) => f.write_str(s),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => {
                if x.fract() == 0.0 {
                    write!(f, "{x:.0}")
                } else {
                    write!(f, "{x:.3}")
                }
            }
            Value::Time(t) => f.write_str(&format_unix(*t)),
            Value::Duration(d) => f.write_str(&format_duration(*d)),
            Value::Path(p) => f.write_str(&p.join("/")),
            Value::List(items) => {
                let parts: Vec<String> = items.iter().map(ToString::to_string).collect();
                f.write_str(&parts.join(", "))
            }
        }
    }
}

/// Compare strings so that embedded numbers sort numerically ("E2" < "E10").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek(), bi.peek()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let na: String = std::iter::from_fn(|| ai.next_if(char::is_ascii_digit)).collect();
                let nb: String = std::iter::from_fn(|| bi.next_if(char::is_ascii_digit)).collect();
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(_), Some(_)) => {
                let ca = ai.next().unwrap().to_lowercase().next().unwrap_or(' ');
                let cb = bi.next().unwrap().to_lowercase().next().unwrap_or(' ');
                if ca != cb {
                    return ca.cmp(&cb);
                }
            }
        }
    }
}

/// Format Unix seconds as `YYYY-MM-DD HH:MM` (UTC).
pub fn format_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

/// Convert a civil date to Unix seconds (UTC), Howard Hinnant's algorithm.
pub fn unix_from_civil(y: i64, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + i64::from(hh) * 3600 + i64::from(mm) * 60 + i64::from(ss)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

pub fn format_duration(secs: f64) -> String {
    let s = secs.round() as i64;
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 { format!("{h}h{m:02}m") } else { format!("{m}m{s:02}s") }
}

/// Human-readable byte size (binary units).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_roundtrip() {
        let values = [
            Value::Str("x".into()),
            Value::Int(-3),
            Value::Float(23.976),
            Value::Time(1_700_000_000),
            Value::Duration(42.5),
            Value::Path(vec!["a".into(), "b c".into(), "f.mkv".into()]),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
        ];
        for v in values {
            let (k, i, r, t) = v.to_sql();
            assert_eq!(Value::from_sql(k, i, r, t), Some(v));
        }
    }

    #[test]
    fn natural_ordering() {
        assert_eq!(natural_cmp("Episode 2", "Episode 10"), Ordering::Less);
        assert_eq!(natural_cmp("a", "B"), Ordering::Less);
        assert_eq!(natural_cmp("x09", "x9"), Ordering::Equal);
    }

    #[test]
    fn civil_dates() {
        assert_eq!(format_unix(0), "1970-01-01 00:00");
        let t = unix_from_civil(2024, 2, 29, 13, 37, 0);
        assert_eq!(format_unix(t), "2024-02-29 13:37");
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KiB");
    }
}
