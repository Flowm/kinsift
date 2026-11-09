//! End-to-end: scan a synthetic tree, query it, rescan after changes.

use std::fs;
use std::path::Path;

use kinsift_core::config::Config;
use kinsift_core::filter::Filter;
use kinsift_core::index::Index;
use kinsift_core::provider::Priority;
use kinsift_core::query::build_tree;
use kinsift_core::scan::{ScanOptions, scan_root};
use kinsift_core::value::Value;

fn touch(root: &Path, rel: &str, bytes: usize) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, vec![0u8; bytes]).unwrap();
}

fn setup() -> (tempfile::TempDir, Index, Vec<Box<dyn kinsift_core::provider::Provider>>, Priority) {
    let dir = tempfile::tempdir().unwrap();
    let providers = kinsift_providers::default_providers();
    let priority = Priority::new(Config::defaults().providers.priority, &providers);
    (dir, Index::open_in_memory().unwrap(), providers, priority)
}

#[test]
fn scan_query_rescan() {
    let (dir, mut index, providers, priority) = setup();
    let root = dir.path();
    touch(root, "Ember Coast/Season 1/Ember.Coast.S01E01.720p.HDTV.x264.mkv", 10);
    touch(root, "Ember Coast/Season 1/Ember.Coast.S01E01.1080p.WEB.x265.mkv", 20);
    touch(root, "Ember Coast/Season 1/Ember.Coast.S01E02.1080p.WEB.x265.mkv", 20);
    touch(root, "Ember Coast/Season 1/notes.txt", 1);
    let opts = ScanOptions::default();
    let r = scan_root(&mut index, &providers, &priority, root, &opts, &|_| {}).unwrap();
    assert_eq!((r.found, r.new, r.pruned), (4, 4, 0));

    let filter = Filter::parse("media.type = episode").unwrap();
    let entries = index.load_entries(Some(&filter), None).unwrap();
    assert_eq!(entries.len(), 3);
    let tree = build_tree(&entries, &["media.title".into(), "media.season".into(), "media.episode".into()], true);
    assert_eq!(tree.dup_groups, 1);
    assert_eq!(tree.file_count, 2);

    // Unchanged rescan reads nothing.
    let r = scan_root(&mut index, &providers, &priority, root, &opts, &|_| {}).unwrap();
    assert_eq!((r.new, r.changed, r.extracted), (0, 0, 0));

    // A changed file is re-read.
    touch(root, "Ember Coast/Season 1/Ember.Coast.S01E02.1080p.WEB.x265.mkv", 25);
    let r = scan_root(&mut index, &providers, &priority, root, &opts, &|_| {}).unwrap();
    assert_eq!((r.changed, r.extracted), (1, 1));

    // One vanished file out of four is pruned (small roots skip the ratio guard).
    fs::remove_file(root.join("Ember Coast/Season 1/notes.txt")).unwrap();
    let r = scan_root(&mut index, &providers, &priority, root, &opts, &|_| {}).unwrap();
    assert_eq!(r.pruned, 1);
    assert!(r.prune_skipped.is_none());
}

#[test]
fn prune_guard_keeps_files_when_most_vanish() {
    let (dir, mut index, providers, priority) = setup();
    let root = dir.path();
    for i in 0..20 {
        touch(root, &format!("Show/Season 1/Show.S01E{i:02}.mkv"), 1);
    }
    let opts = ScanOptions::default();
    scan_root(&mut index, &providers, &priority, root, &opts, &|_| {}).unwrap();
    for i in 0..10 {
        fs::remove_file(root.join(format!("Show/Season 1/Show.S01E{i:02}.mkv"))).unwrap();
    }
    let r = scan_root(&mut index, &providers, &priority, root, &opts, &|_| {}).unwrap();
    assert_eq!(r.pruned, 0);
    assert!(r.prune_skipped.unwrap().contains("10 of 20"));
    assert_eq!(index.load_entries(None, None).unwrap().len(), 20, "index keeps the vanished files");

    let forced = ScanOptions { force_prune: true };
    let r = scan_root(&mut index, &providers, &priority, root, &forced, &|_| {}).unwrap();
    assert_eq!(r.pruned, 10);
}

#[test]
fn inspect_sources_and_priority() {
    let (dir, mut index, providers, priority) = setup();
    let root = dir.path();
    touch(root, "Glass Orchard (2021)/Glass.Orchard.2021.2160p.UHD.x265.mkv", 5);
    scan_root(&mut index, &providers, &priority, root, &ScanOptions::default(), &|_| {}).unwrap();
    let entry =
        index.find_by_path(&root.join("Glass Orchard (2021)/Glass.Orchard.2021.2160p.UHD.x265.mkv")).unwrap().unwrap();
    assert_eq!(entry.get("video.resolution"), Some(&Value::Int(2160)));
    assert_eq!(entry.get("media.year"), Some(&Value::Int(2021)));
    let sources = index.sources(entry.id).unwrap();
    assert!(sources.iter().any(|s| s.key == "video.resolution" && s.provider == "name" && s.effective));
    let raw = index.extractions(entry.id).unwrap();
    assert!(raw.iter().any(|x| x.provider == "name" && x.raw.is_some()));
}
