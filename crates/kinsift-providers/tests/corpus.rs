//! Accuracy of the name provider on your own library: a private corpus.
//!
//! The corpus is a set of `*.txt` files with one absolute path per line,
//! stored OUTSIDE the repository (default `~/.local/share/kinsift/corpus/`,
//! or `$KINSIFT_CORPUS`). Create it with e.g.
//!
//! ```sh
//! mkdir -p ~/.local/share/kinsift/corpus
//! find /mnt/series -type f > ~/.local/share/kinsift/corpus/series.txt
//! ```
//!
//! Ground truth comes from the usual media library folder layout:
//! `<Show>/<Season N>/<file>` and `<Title> (<Year>)/<file>`.
//! The test prints aggregate numbers only, never a path.
//!
//! Run with `cargo test -p kinsift-providers --test corpus -- --ignored --nocapture`.

use std::path::{Path, PathBuf};

use kinsift_core::normalize::title_key;
use kinsift_core::provider::VIDEO_EXTENSIONS;
use kinsift_core::value::Value;
use kinsift_providers::name;
use regex::Regex;

#[derive(Default)]
struct Tally {
    n: usize,
    title: usize,
    number: usize,
    kind: usize,
}

impl Tally {
    fn line(&self, what: &str, number_label: &str) -> String {
        let pct = |x: usize| if self.n == 0 { 0.0 } else { x as f64 * 100.0 / self.n as f64 };
        format!(
            "{what:<9} n={:<6} title {:5.1}%   {number_label} {:5.1}%   type {:5.1}%",
            self.n,
            pct(self.title),
            pct(self.number),
            pct(self.kind)
        )
    }

    fn title_rate(&self) -> f64 {
        self.title as f64 / self.n.max(1) as f64
    }
}

fn corpus_dir() -> Option<PathBuf> {
    std::env::var_os("KINSIFT_CORPUS")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|d| d.join("kinsift").join("corpus")))
}

fn get(e: &kinsift_core::provider::Extraction, key: &str) -> Option<Value> {
    e.sets.iter().flat_map(|s| s.keys().map(|(k, v)| (k, v.clone()))).find(|(k, _)| k == key).map(|(_, v)| v)
}

#[test]
#[ignore = "needs a private corpus outside the repo"]
fn private_corpus() {
    let Some(dir) = corpus_dir().filter(|d| d.is_dir()) else {
        eprintln!("no corpus directory; see the module docs");
        return;
    };
    let season_dir = Regex::new(r"(?i)^(?:season|staffel|saison|temporada)[ ._-]*(\d{1,3})$").unwrap();
    let specials_dir = Regex::new(r"(?i)^specials?$").unwrap();
    let movie_dir = Regex::new(r"^(.+?) \(((?:19|20)\d{2})\)$").unwrap();
    let year_suffix = Regex::new(r"\s*\((?:19|20)\d{2}\)$").unwrap();

    let mut episodes = Tally::default();
    let mut movies = Tally::default();
    let mut skipped = 0usize;
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    files.sort();

    for list in files {
        let text = std::fs::read_to_string(&list).unwrap();
        for line in text.lines() {
            let path = Path::new(line.trim());
            let is_video = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
            let parent = path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
            let grand = path
                .parent()
                .and_then(Path::parent)
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned());
            let (Some(parent), Some(grand), true) = (parent, grand, is_video) else {
                skipped += 1;
                continue;
            };
            let season = season_dir
                .captures(&parent)
                .map(|c| c[1].parse::<i64>().unwrap())
                .or_else(|| specials_dir.is_match(&parent).then_some(0));
            let e = name::parse(path);
            let title = get(&e, "media.title").map(|v| title_key(&v.to_string()));
            let kind = get(&e, "media.type").map(|v| v.to_string());
            if let Some(season) = season {
                let expected = title_key(&year_suffix.replace(&grand, ""));
                episodes.n += 1;
                episodes.title += usize::from(title.as_deref() == Some(expected.as_str()));
                episodes.number += usize::from(get(&e, "media.season") == Some(Value::Int(season)));
                episodes.kind += usize::from(kind.as_deref() == Some("episode"));
            } else if let Some(c) = movie_dir.captures(&parent) {
                movies.n += 1;
                movies.title += usize::from(title.as_deref() == Some(title_key(&c[1]).as_str()));
                movies.number += usize::from(get(&e, "media.year") == Some(Value::Int(c[2].parse().unwrap())));
                movies.kind += usize::from(kind.as_deref() == Some("movie"));
            } else {
                skipped += 1;
            }
        }
    }
    println!("{}", episodes.line("episodes", "season"));
    println!("{}", movies.line("movies", "year  "));
    println!("skipped  n={skipped} (not in a recognizable layout)");
    for (t, what) in [(&episodes, "episode"), (&movies, "movie")] {
        if t.n >= 50 {
            assert!(t.title_rate() >= 0.85, "{what} title accuracy dropped below 85%");
        }
    }
}
