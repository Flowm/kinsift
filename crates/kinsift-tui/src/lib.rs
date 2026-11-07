//! The kinsift terminal UI.

pub mod app;
mod ui;

use std::sync::mpsc;
use std::time::Duration;

use kinsift_core::index::Index;
use kinsift_core::scan::{ScanEvent, ScanOptions, scan_root};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

pub use app::{App, TuiContext};

/// Draw one frame (exposed for headless tests).
pub fn draw(f: &mut ratatui::Frame, app: &mut App) {
    ui::draw(f, app);
}
use app::{ScanJob, ScanMsg};

/// Run the TUI until the user quits.
pub fn run(ctx: TuiContext) -> anyhow::Result<()> {
    let mut app = App::new(ctx)?;
    let mut terminal = ratatui::init();
    // ratatui's hook restores the terminal on panic. A panic on a background
    // thread must not do that while the UI keeps running; poll_scan reports
    // the failed job instead.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            hook(info);
        } else {
            log::error!("background job panicked: {info}");
        }
    }));
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(Duration::from_millis(150))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key);
        }
        if app.request_rescan {
            app.request_rescan = false;
            start_scan(app);
        }
        poll_scan(app);
    }
    Ok(())
}

/// Rescan all known roots on a background thread with its own connection.
pub fn start_scan(app: &mut App) {
    if app.scan.is_some() {
        return app.set_status("a scan is already running");
    }
    let mut roots: Vec<_> = app.ctx.index.roots().map(|r| r.into_iter().map(|r| r.path).collect()).unwrap_or_default();
    for r in &app.ctx.config.roots {
        let p = r.path.canonicalize().unwrap_or_else(|_| r.path.clone());
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    if roots.is_empty() {
        return app.set_status("no roots known; run `kinsift scan <dir>` first");
    }
    let (tx, rx) = mpsc::channel();
    let path = app.ctx.index_path.clone();
    let providers = app.ctx.providers.clone();
    let priority = app.ctx.priority.clone();
    std::thread::spawn(move || {
        let result = (|| -> Result<Vec<String>, String> {
            let mut index = Index::open(&path).map_err(|e| e.to_string())?;
            let mut summary = Vec::new();
            let progress = |ev: ScanEvent| {
                let _ = tx.send(ScanMsg::Event(ev));
            };
            for root in roots {
                match scan_root(&mut index, &providers, &priority, &root, &ScanOptions::default(), &progress) {
                    Ok(r) => {
                        let mut s =
                            format!("{}: {} new, {} changed, {} pruned", root.display(), r.new, r.changed, r.pruned);
                        if let Some(reason) = r.prune_skipped {
                            s.push_str(&format!(" (kept vanished files: {reason})"));
                        }
                        summary.push(s);
                    }
                    Err(e) => summary.push(format!("{}: {e}", root.display())),
                }
            }
            Ok(summary)
        })();
        let _ = tx.send(ScanMsg::Finished(result));
    });
    app.scan = Some(ScanJob { rx, progress: "starting scan".into() });
}

fn poll_scan(app: &mut App) {
    let Some(job) = app.scan.as_mut() else { return };
    let mut finished = None;
    loop {
        let msg = match job.rx.try_recv() {
            Ok(msg) => msg,
            Err(mpsc::TryRecvError::Empty) => break,
            // The job's thread ended without reporting back (it panicked).
            Err(mpsc::TryRecvError::Disconnected) => {
                finished.get_or_insert(Err("the background job stopped unexpectedly".to_owned()));
                break;
            }
        };
        match msg {
            ScanMsg::Event(ScanEvent::Walking { root, found }) => {
                job.progress = format!("walking {}: {found}", root.display())
            }
            ScanMsg::Event(ScanEvent::Extracting { root, done, total }) => {
                job.progress = format!("reading {}: {done}/{total}", root.display())
            }
            ScanMsg::Event(ScanEvent::Done(_)) => {}
            ScanMsg::Finished(r) => finished = Some(r),
        }
    }
    if let Some(result) = finished {
        app.scan = None;
        match result {
            Ok(summary) => {
                let _ = app.reload();
                app.set_status(format!("scan done: {}", summary.join("; ")));
            }
            Err(e) => app.set_status(format!("scan failed: {e}")),
        }
    }
}
