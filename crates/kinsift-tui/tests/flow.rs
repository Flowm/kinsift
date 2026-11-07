//! Headless end-to-end: keep-rule → review → quarantine → undo.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use kinsift_core::config::{Config, RootConfig};
use kinsift_core::index::Index;
use kinsift_core::provider::Priority;
use kinsift_core::scan::{ScanOptions, scan_root};
use kinsift_tui::app::Modal;
use kinsift_tui::{App, TuiContext};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const SEASON: &str = "Ember Coast/Season 1";

fn touch(root: &Path, rel: &str, bytes: usize) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, vec![1u8; bytes]).unwrap();
}

fn app(root: &Path) -> App {
    touch(root, &format!("{SEASON}/Ember.Coast.S01E01.720p.HDTV.x264.mkv"), 10);
    touch(root, &format!("{SEASON}/Ember.Coast.S01E01.1080p.WEB.x265.mkv"), 20);
    touch(root, &format!("{SEASON}/Ember.Coast.S01E02.1080p.WEB.x265.mkv"), 20);
    let mut config = Config::defaults();
    config.roots.push(RootConfig { path: root.to_path_buf(), delete: Some("quarantine".into()), quarantine: None });
    let providers = kinsift_providers::default_providers();
    let priority = Priority::new(config.providers.priority.clone(), &providers);
    let index_path = root.parent().unwrap().join(format!("{}.db", root.file_name().unwrap().to_string_lossy()));
    let mut index = Index::open(&index_path).unwrap();
    scan_root(&mut index, &providers, &priority, root, &ScanOptions::default(), &|_| {}).unwrap();
    App::new(TuiContext {
        config,
        index,
        index_path,
        providers: Arc::new(providers),
        priority,
        start_view: Some("series".into()),
    })
    .unwrap()
}

fn key(app: &mut App, c: KeyCode) {
    app.on_key(KeyEvent::new(c, KeyModifiers::NONE));
}

fn screen(app: &mut App) -> String {
    let mut term = Terminal::new(TestBackend::new(140, 30)).unwrap();
    term.draw(|f| kinsift_tui::draw(f, app)).unwrap();
    let buf = term.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_owned()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn keep_rule_review_quarantine_undo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let mut app = app(&root);

    let s = screen(&mut app);
    assert!(s.contains("Ember Coast"), "{s}");
    assert!(s.contains("1 duplicate groups"), "{s}");

    key(&mut app, KeyCode::Char('E'));
    assert!(screen(&mut app).contains("Episode 1"));

    // Keep-rule picker: the view's default (lower-resolution) is first.
    key(&mut app, KeyCode::Char('K'));
    assert!(matches!(app.modal, Some(Modal::Picker { .. })));
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.marked.len(), 1, "the 720p copy is marked");

    key(&mut app, KeyCode::Char('D'));
    let s = screen(&mut app);
    assert!(s.contains("Remove 1 files"), "{s}");
    assert!(s.contains("quarantine"), "{s}");
    key(&mut app, KeyCode::Enter);
    assert!(app.modal.is_none());
    let low = root.join(format!("{SEASON}/Ember.Coast.S01E01.720p.HDTV.x264.mkv"));
    assert!(!low.exists());
    assert!(root.join(".kinsift-trash").is_dir());
    assert!(screen(&mut app).contains("moved to quarantine"));
    assert!(app.marked.is_empty());

    key(&mut app, KeyCode::Char('u'));
    assert!(matches!(app.modal, Some(Modal::ConfirmUndo { .. })));
    key(&mut app, KeyCode::Char('y'));
    assert!(low.exists(), "undo restores the file");
    assert!(screen(&mut app).contains("restored 1 files"));
    assert_eq!(app.tree.dup_groups, 1, "the restored file is back in the view");
}

#[test]
fn removing_every_copy_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let mut app = app(&root);
    app.dup_only = true;
    app.rebuild();
    key(&mut app, KeyCode::Char('E'));
    // Mark both files of the duplicate group by hand.
    for id in app.tree.all_files().iter().map(|&i| app.entries[i].id).collect::<Vec<_>>() {
        app.marked.insert(id);
    }
    key(&mut app, KeyCode::Char('D'));
    let s = screen(&mut app);
    assert!(s.contains("refused") && s.contains("remaining copy"), "{s}");
    assert!(s.contains("Remove 0 files"), "{s}");
    key(&mut app, KeyCode::Enter);
    assert_eq!(fs::read_dir(root.join(SEASON)).unwrap().count(), 3, "nothing was removed");
}

#[test]
fn mark_by_value_and_filter() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let mut app = app(&root);
    // Cursor is on the show; `m` offers field values.
    key(&mut app, KeyCode::Char('m'));
    let Some(Modal::Picker { items, .. }) = &app.modal else { panic!("expected picker") };
    let pos = items.iter().position(|i| i.starts_with("video.resolution = 720p")).expect("resolution option");
    for _ in 0..pos {
        key(&mut app, KeyCode::Down);
    }
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.marked.len(), 1);

    key(&mut app, KeyCode::Char('/'));
    for c in "video.resolution >= 1080".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.entries.len(), 2);
    assert!(app.marked.is_empty(), "marks outside the filter are dropped");
}

fn pick(app: &mut App, opener: char, item: &str) {
    key(app, KeyCode::Char(opener));
    let Some(Modal::Picker { items, cursor, .. }) = &app.modal else { panic!("expected picker") };
    let (cursor, pos) = (*cursor, items.iter().position(|i| i.contains(item)).unwrap_or_else(|| panic!("no {item}")));
    let k = if pos > cursor { KeyCode::Down } else { KeyCode::Up };
    for _ in 0..pos.abs_diff(cursor) {
        key(app, k);
    }
    key(app, KeyCode::Enter);
}

#[test]
fn switching_to_an_empty_view_or_filter_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let mut app = app(&root);
    key(&mut app, KeyCode::Enter); // expand, so the cursor row has a deep path
    key(&mut app, KeyCode::Down);
    pick(&mut app, 'v', "photos-by-date");
    assert!(app.entries.is_empty() && app.rows.is_empty());
    screen(&mut app);
    for k in [KeyCode::Enter, KeyCode::Char('l'), KeyCode::Char('h'), KeyCode::Char('e'), KeyCode::Char('m')] {
        key(&mut app, k);
        app.modal = None;
    }
    pick(&mut app, 'v', "series");
    assert_eq!(app.entries.len(), 3);
    key(&mut app, KeyCode::Char('/'));
    for c in "fs.size > 1TiB".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    key(&mut app, KeyCode::Enter);
    assert!(app.rows.is_empty());
    screen(&mut app);
}

#[test]
fn sorting_keeps_the_cursor_on_its_group() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    touch(&root, "Ash Harbor/Season 1/Ash.Harbor.S01E01.mkv", 1);
    let mut app = app(&root);
    key(&mut app, KeyCode::Down);
    assert!(screen(&mut app).contains("Ember Coast"));
    let label = |app: &App| match &app.rows[app.cursor].kind {
        kinsift_tui::app::RowKind::Group(p) => app.node(p).label.clone(),
        _ => panic!("expected a group row"),
    };
    assert_eq!((app.cursor, label(&app).as_str()), (1, "Ember Coast"));
    pick(&mut app, 's', "size");
    assert_eq!((app.cursor, label(&app).as_str()), (0, "Ember Coast"));
}

#[test]
fn a_reload_closes_dialogs_that_hold_file_indices() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let mut app = app(&root);
    key(&mut app, KeyCode::Char('m'));
    assert!(matches!(app.modal, Some(Modal::Picker { .. })));
    app.reload().unwrap(); // e.g. a background rescan finished
    assert!(app.modal.is_none());
    assert!(app.marked.is_empty());
}

#[test]
fn marking_a_file_keeps_the_cursor_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let mut app = app(&root);
    key(&mut app, KeyCode::Char('E'));
    let first_file = app.rows.iter().position(|r| matches!(r.kind, kinsift_tui::app::RowKind::File(_))).unwrap();
    while app.cursor < first_file {
        key(&mut app, KeyCode::Down);
    }
    key(&mut app, KeyCode::Char('m'));
    assert_eq!((app.cursor, app.marked.len()), (first_file, 1));
    key(&mut app, KeyCode::Char(' '));
    assert_eq!((app.cursor, app.marked.len()), (first_file, 0), "a second press unmarks");
}

#[test]
fn files_are_not_changed_while_a_background_job_runs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let mut app = app(&root);
    let (_tx, rx) = std::sync::mpsc::channel();
    app.scan = Some(kinsift_tui::app::ScanJob { rx, progress: "hashing".into() });
    key(&mut app, KeyCode::Char('E'));
    while !matches!(app.rows[app.cursor].kind, kinsift_tui::app::RowKind::File(_)) {
        key(&mut app, KeyCode::Down);
    }
    key(&mut app, KeyCode::Char('d'));
    assert!(app.modal.is_none(), "no review while the job runs");
    assert!(screen(&mut app).contains("wait for the background job"));
}
