//! Compare the name provider with guessit's own test fixtures.
//!
//! guessit is LGPL-3.0, so its fixtures are not copied into this repository.
//! Fetch them first:
//!
//! ```sh
//! scripts/fetch-guessit-fixtures.sh   # writes target/guessit-fixtures/*.yml
//! cargo test -p kinsift-providers --test guessit_fixtures -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kinsift_core::normalize::title_key;
use kinsift_core::value::Value;
use kinsift_providers::name;

const FIELDS: [&str; 5] = ["title", "year", "season", "episode", "type"];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/guessit-fixtures")
}

fn get(e: &kinsift_core::provider::Extraction, key: &str) -> Option<Value> {
    e.sets.iter().flat_map(|s| s.keys().map(|(k, v)| (k, v.clone()))).find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Expected values of one case: field -> list of scalars.
type Expected = BTreeMap<String, Vec<String>>;

fn unquote(s: &str) -> String {
    s.trim().trim_matches(|c| c == '"' || c == '\'').to_owned()
}

/// Minimal reader for guessit's fixture format (`? input` / `: field: value`).
/// Strict YAML parsers reject these files because they repeat keys.
fn parse_fixtures(text: &str) -> Vec<(Vec<String>, Expected)> {
    let mut cases = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut current: Option<Expected> = None;
    let mut list_field: Option<String> = None;
    let mut flush = |keys: &mut Vec<String>, current: &mut Option<Expected>| {
        if let Some(exp) = current.take()
            && !keys.is_empty()
        {
            cases.push((std::mem::take(keys), exp));
        }
    };
    for line in text.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if let Some(k) = line.strip_prefix("? ") {
            if current.is_some() {
                flush(&mut keys, &mut current);
            }
            keys.push(unquote(k));
            continue;
        }
        let body = if let Some(rest) = line.strip_prefix(": ") {
            current.get_or_insert_with(Expected::new);
            rest
        } else if line.starts_with("  ") && current.is_some() {
            line.trim_start()
        } else {
            flush(&mut keys, &mut current);
            keys.clear();
            continue;
        };
        let Some(exp) = current.as_mut() else { continue };
        if let Some(item) = body.strip_prefix("- ") {
            if let Some(f) = &list_field {
                exp.entry(f.clone()).or_default().push(unquote(item));
            }
            continue;
        }
        let Some((field, value)) = body.split_once(':') else { continue };
        let (field, value) = (field.trim().to_owned(), value.trim());
        list_field = None;
        if value.is_empty() {
            list_field = Some(field);
        } else if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            exp.insert(field, inner.split(',').map(unquote).collect());
        } else {
            exp.insert(field, vec![unquote(value)]);
        }
    }
    flush(&mut keys, &mut current);
    cases
}

fn value_ints(v: &Value) -> Vec<i64> {
    match v {
        Value::List(items) => items.iter().filter_map(Value::as_i64).collect(),
        other => other.as_i64().into_iter().collect(),
    }
}

#[test]
#[ignore = "needs guessit fixtures in target/guessit-fixtures"]
fn guessit_fixture_agreement() {
    let dir = fixture_dir();
    let Ok(read) = std::fs::read_dir(&dir) else {
        eprintln!("no fixtures in {}; run scripts/fetch-guessit-fixtures.sh", dir.display());
        return;
    };
    let mut files: Vec<PathBuf> = read.filter_map(Result::ok).map(|e| e.path()).collect();
    files.sort();
    for file in files {
        let cases = parse_fixtures(&std::fs::read_to_string(&file).unwrap());
        let mut stats: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        let mut all = (0usize, 0usize);
        for (inputs, exp) in cases {
            for input in inputs {
                if input.starts_with("__") || input.starts_with('-') {
                    continue;
                }
                let e = name::parse(Path::new(input.trim_start_matches('+')));
                let mut all_ok = true;
                let mut any = false;
                for field in FIELDS {
                    let Some(want) = exp.get(field) else { continue };
                    let got = get(&e, &format!("name.{field}"));
                    let ok = match field {
                        "title" => Some(title_key(&want[0])) == got.map(|v| title_key(&v.to_string())),
                        "type" => Some(want[0].as_str()) == got.as_ref().and_then(Value::as_str),
                        _ => {
                            let want: Option<Vec<i64>> = want.iter().map(|w| w.parse().ok()).collect();
                            want == got.map(|v| value_ints(&v))
                        }
                    };
                    let s = stats.entry(field).or_default();
                    s.0 += 1;
                    s.1 += usize::from(ok);
                    all_ok &= ok;
                    any = true;
                }
                if any {
                    all.0 += 1;
                    all.1 += usize::from(all_ok);
                }
            }
        }
        let name = file.file_name().unwrap().to_string_lossy();
        let pct = |(n, k): (usize, usize)| if n == 0 { 0.0 } else { k as f64 * 100.0 / n as f64 };
        let per_field: Vec<String> = stats.iter().map(|(f, s)| format!("{f} {:.1}%", pct(*s))).collect();
        println!("{name:<14} cases {:>4}: all fields {:5.1}% | {}", all.0, pct(all), per_field.join(", "));
    }
}

#[test]
fn fixture_reader() {
    let text = "? A.S01E02.mkv\n? B.S01E02.mkv\n: title: A\n  episode: [2, 3]\n  other:\n  - x\n  - y\n\n? C.2019.mkv\n: year: 2019\n";
    let cases = parse_fixtures(text);
    assert_eq!(cases.len(), 2);
    assert_eq!(cases[0].0, ["A.S01E02.mkv", "B.S01E02.mkv"]);
    assert_eq!(cases[0].1["episode"], ["2", "3"]);
    assert_eq!(cases[0].1["other"], ["x", "y"]);
    assert_eq!(cases[1].1["year"], ["2019"]);
}
