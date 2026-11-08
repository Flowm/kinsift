//! Rendering (the "view" half of the Elm-style loop).

use kinsift_core::actions::StrategyKind;
use kinsift_core::query::Node;
use kinsift_core::value::{format_size, format_unix};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};

use crate::app::{App, Modal, RowKind, Summary};

const HELP: &[(&str, &str)] = &[
    ("↑ ↓ j k  PgUp PgDn  g G", "move"),
    ("→ l / ← h / Enter", "expand / collapse (or go to parent) / toggle"),
    ("e  E  c  C", "expand subtree / everything, collapse subtree / everything"),
    ("v", "choose a view"),
    ("b", "group by other fields"),
    ("/", "filter (e.g. video.resolution < 1080 and fs.size > 1GiB)"),
    ("f", "duplicate groups only on/off"),
    ("s", "sort groups"),
    ("i  Tab", "details pane: every field and which provider set it"),
    ("m  Space", "mark file / on a group: mark files by field value"),
    ("K", "apply a keep-rule to the duplicate groups under the cursor"),
    ("D  Ctrl+D", "review and remove marked files"),
    ("d  Del", "review and remove the file under the cursor"),
    ("Ctrl+U", "unmark all"),
    ("r", "rename file"),
    ("u", "undo the last batch"),
    ("R", "rescan all roots in the background"),
    ("H", "hash files of this view that share a size (for the by-hash view)"),
    ("q", "quit"),
];

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, status] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(2)]).areas(f.area());
    draw_header(f, app, header);
    let (tree_area, detail_area) = if app.detail {
        let [a, b] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(body);
        (a, Some(b))
    } else {
        (body, None)
    };
    app.height = tree_area.height as usize;
    draw_tree(f, app, tree_area);
    if let Some(area) = detail_area {
        draw_detail(f, app, area);
    }
    draw_status(f, app, status);
    draw_modal(f, app);
}

/// Palette, close to Textual's default dark theme that mdupes used.
mod palette {
    use ratatui::style::Color;
    pub const BAR_BG: Color = Color::Rgb(0x1f, 0x2b, 0x4d);
    pub const BAR_FG: Color = Color::Rgb(0xe0, 0xe0, 0xe0);
    pub const KEY: Color = Color::Rgb(0xff, 0xb8, 0x3d);
    pub const CURSOR_BG: Color = Color::Rgb(0x3a, 0x3d, 0x45);
    pub const GUIDE: Color = Color::Rgb(0x5c, 0x5f, 0x66);
    pub const DIM: Color = Color::Rgb(0x9a, 0x9d, 0xa3);
    pub const LABEL: Color = Color::Rgb(0x4e, 0xc9, 0xd6);
    pub const RES: Color = Color::Rgb(0xd7, 0x99, 0x21);
    pub const DUP: Color = Color::Rgb(0xff, 0x5c, 0x6c);
    pub const MARK: Color = Color::Rgb(0xff, 0x5c, 0x6c);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let view = app.effective_view();
    let mut text = format!("kinsift — {}", view.name);
    text.push_str(&format!("  ·  by {}", view.group_by.join(" › ")));
    text.push_str(if app.dup_only { "  ·  duplicates only" } else { "  ·  all groups" });
    text.push_str(&format!("  ·  sort: {}", app.sort.label()));
    if let Some(filter) = &view.filter {
        text.push_str(&format!("  ·  where {filter}"));
    }
    let style = Style::new().bg(palette::BAR_BG).fg(palette::BAR_FG);
    f.render_widget(Paragraph::new(Line::from(text).centered()).style(style), area);
}

/// Display name for a codec value, e.g. `hevc` -> `H.265`.
fn codec_label(c: &str) -> String {
    match c {
        "h264" | "avc" => "H.264".into(),
        "hevc" | "h265" => "H.265".into(),
        "av1" => "AV1".into(),
        "vp9" => "VP9".into(),
        "vp8" => "VP8".into(),
        "mpeg2video" | "mpeg2" => "MPEG-2".into(),
        "mpeg4" => "MPEG-4".into(),
        "vc1" => "VC-1".into(),
        "xvid" => "Xvid".into(),
        "divx" => "DivX".into(),
        other => other.to_uppercase(),
    }
}

/// `(3 files, 10.9 GiB) [720p, 1080p] [H.264] 2 duplicates`
fn summary_spans(files: Option<usize>, size: u64, summary: Option<&Summary>, redundant: usize) -> Vec<Span<'static>> {
    let dim = Style::new().fg(palette::DIM);
    let mut spans = Vec::new();
    let counts = match files {
        Some(n) => format!(" ({n} {}, {})", if n == 1 { "file" } else { "files" }, format_size(size)),
        None => format!(" ({})", format_size(size)),
    };
    spans.push(Span::styled(counts, dim));
    if let Some(s) = summary {
        if !s.resolutions.is_empty() {
            let r: Vec<String> = s.resolutions.iter().map(|r| format!("{r}p")).collect();
            spans.push(Span::styled(format!(" [{}]", r.join(", ")), Style::new().fg(palette::RES)));
        }
        if !s.codecs.is_empty() {
            let mut c: Vec<String> = s.codecs.iter().map(|c| codec_label(c)).collect();
            c.sort();
            c.dedup();
            spans.push(Span::styled(format!(" [{}]", c.join(", ")), dim));
        }
    }
    if redundant > 0 {
        let word = if redundant == 1 { "duplicate" } else { "duplicates" };
        spans.push(Span::styled(format!(" {redundant} {word}"), Style::new().fg(palette::DUP)));
    }
    spans
}

fn guides(row: &crate::app::Row) -> String {
    let mut s = String::new();
    for &last in &row.last[..row.depth] {
        s.push_str(if last { "    " } else { "│   " });
    }
    s.push_str(if row.last.last().copied().unwrap_or(true) { "└── " } else { "├── " });
    s
}

fn draw_tree(f: &mut Frame, app: &App, area: Rect) {
    if app.rows.is_empty() {
        let msg = if app.entries.is_empty() {
            "No files match. Scan a root with `kinsift scan <dir>`, or change the view (v) or filter (/)."
        } else {
            "No groups to show (try f to show all groups)."
        };
        f.render_widget(Paragraph::new(msg).fg(palette::DIM).wrap(Wrap { trim: true }), area);
        return;
    }
    let scroll = app.rows.len() > area.height as usize;
    let width = area.width.saturating_sub(u16::from(scroll)) as usize;
    let mut lines = Vec::new();
    for (i, row) in app.rows.iter().enumerate().skip(app.offset).take(area.height as usize) {
        let mut spans = vec![Span::styled(guides(row), Style::new().fg(palette::GUIDE))];
        match &row.kind {
            RowKind::Group(path) => {
                let node = app.node(path);
                let arrow = if app.expanded.contains(&node.id) { "▼ " } else { "▶ " };
                spans.push(Span::raw(arrow));
                spans.push(Span::styled(node.label.clone(), Style::new().fg(palette::LABEL)));
                spans.extend(summary_spans(
                    Some(node.file_count),
                    node.size,
                    app.summaries.get(&node.id),
                    node.redundant,
                ));
                let marked = if app.marked.is_empty() { 0 } else { marked_in(app, node) };
                if marked > 0 {
                    spans.push(Span::styled(format!("  ✗ {marked} marked"), Style::new().fg(palette::MARK).bold()));
                }
            }
            RowKind::File(fi) => {
                let e = &app.entries[*fi];
                let marked = app.marked.contains(&e.id);
                let name = e.rel.rsplit('/').next().unwrap_or(&e.rel).to_owned();
                if marked {
                    spans.push(Span::styled("✗ ", Style::new().fg(palette::MARK).bold()));
                    spans.push(Span::styled(name, Style::new().fg(palette::MARK).crossed_out()));
                } else {
                    spans.push(Span::styled(name, Style::new().fg(palette::LABEL)));
                }
                spans.extend(summary_spans(None, e.size, Some(&Summary::of(e)), 0));
            }
        }
        let mut line = Line::from(spans);
        if i == app.cursor {
            let used = line.width();
            if used < width {
                line.push_span(Span::raw(" ".repeat(width - used)));
            }
            line = line.style(Style::new().bg(palette::CURSOR_BG).add_modifier(Modifier::BOLD));
        }
        lines.push(line);
    }
    let tree_area = Rect { width: width as u16, ..area };
    f.render_widget(Paragraph::new(lines), tree_area);
    if scroll {
        let mut state = ScrollbarState::new(app.rows.len().saturating_sub(area.height as usize)).position(app.offset);
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("▕"))
            .thumb_symbol("█")
            .track_style(Style::new().fg(palette::GUIDE))
            .thumb_style(Style::new().fg(palette::LABEL));
        f.render_stateful_widget(bar, area, &mut state);
    }
}

fn marked_in(app: &App, node: &Node) -> usize {
    node.all_files().iter().filter(|&&i| app.marked.contains(&app.entries[i].id)).count()
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().borders(Borders::LEFT).title(" details ");
    let mut lines: Vec<Line> = Vec::new();
    if let Some(entry) = app.current_file().cloned() {
        lines.push(Line::from(entry.abs().display().to_string()).bold());
        lines.push(Line::from(format!(
            "{}  ·  modified {}",
            format_size(entry.size),
            format_unix(entry.mtime_ns / 1_000_000_000)
        )));
        lines.push(Line::raw(""));
        let sources = app.detail_sources().map(<[_]>::to_vec).unwrap_or_default();
        let width = entry.fields.keys().map(String::len).max().unwrap_or(8).min(22);
        for (key, value) in &entry.fields {
            if key.starts_with("fs.") && key != "fs.ext" {
                continue;
            }
            let from = sources.iter().find(|s| &s.key == key && s.effective).map(|s| s.provider.clone());
            let mut spans = vec![
                Span::styled(format!("{key:<width$} "), Style::new().fg(Color::Cyan)),
                Span::raw(value.to_string()),
                Span::styled(
                    format!("  {}", from.unwrap_or_else(|| "kinsift".into())),
                    Style::new().fg(Color::DarkGray),
                ),
            ];
            for other in sources.iter().filter(|s| &s.key == key && !s.effective) {
                spans.push(Span::styled(
                    format!("  ({}: {})", other.provider, other.value),
                    Style::new().fg(Color::Magenta),
                ));
            }
            lines.push(Line::from(spans));
        }
    } else if let Some(RowKind::Group(path)) = app.rows.get(app.cursor).map(|r| &r.kind) {
        let node = app.node(path);
        lines.push(Line::from(node.label.clone()).bold());
        lines.push(Line::from(format!("grouped by {}", node.field)));
        lines.push(Line::from(format!("{} files, {}", node.file_count, format_size(node.size))));
        lines.push(Line::from(format!("{} duplicate groups", node.dup_groups)));
        let marked = marked_in(app, node);
        if marked > 0 {
            lines.push(Line::from(format!("{marked} marked")).fg(Color::Red));
        }
    }
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let [l1, l2] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let mut spans = Vec::new();
    if let Some(job) = &app.scan {
        spans.push(Span::styled(format!("⟳ {}  ", job.progress), Style::new().fg(Color::Cyan)));
    }
    match &app.status {
        Some((msg, at)) if at.elapsed().as_secs() < 8 => {
            spans.push(Span::styled(msg.clone(), Style::new().fg(Color::Green)))
        }
        _ => {
            spans.push(Span::raw(format!(
                "{} files · {} · {} duplicate groups",
                app.tree.file_count,
                format_size(app.tree.size),
                app.tree.dup_groups
            )));
            if !app.marked.is_empty() {
                spans.push(Span::styled(
                    format!("  ·  {} marked ({})", app.marked.len(), format_size(app.marked_size())),
                    Style::new().fg(Color::Red),
                ));
            }
        }
    }
    if let Some(q) = &app.quarantine_note {
        spans.push(Span::styled(format!("  ·  {q}"), Style::new().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), l1);
    let bar = Style::new().bg(palette::BAR_BG).fg(palette::BAR_FG);
    let key = Style::new().bg(palette::BAR_BG).fg(palette::KEY).bold();
    let mut hints = Vec::new();
    for (k, label) in FOOTER {
        hints.push(Span::styled(format!(" {k} "), key));
        hints.push(Span::styled(format!("{label} "), bar));
    }
    f.render_widget(Paragraph::new(Line::from(hints)).style(bar), l2);
}

const FOOTER: &[(&str, &str)] = &[
    ("q", "Quit"),
    ("v", "Views"),
    ("/", "Filter"),
    ("f", "Duplicates"),
    ("s", "Sort"),
    ("m", "Mark"),
    ("K", "Keep-rule"),
    ("D", "Review marked"),
    ("d", "Delete"),
    ("r", "Rename"),
    ("u", "Undo"),
    ("i", "Info"),
    ("?", "Help"),
];

fn centered(area: Rect, w_pct: u16, h: u16) -> Rect {
    let w = area.width * w_pct / 100;
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height.saturating_sub(h)) / 2, width: w, height: h }
}

fn draw_modal(f: &mut Frame, app: &App) {
    let Some(modal) = &app.modal else { return };
    let full = f.area();
    match modal {
        Modal::Help => {
            let area = centered(full, 80, HELP.len() as u16 + 4);
            let lines: Vec<Line> = HELP
                .iter()
                .map(|(k, d)| {
                    Line::from(vec![Span::styled(format!("{k:<26}"), Style::new().fg(Color::Cyan)), Span::raw(*d)])
                })
                .collect();
            popup(f, area, " keys (any key closes) ", lines);
        }
        Modal::Message { title, lines } => {
            let area = centered(full, 80, lines.len() as u16 + 4);
            popup(
                f,
                area,
                &format!(" {title} (any key closes) "),
                lines.iter().map(|l| Line::raw(l.clone())).collect(),
            );
        }
        Modal::Picker { title, items, cursor, .. } => {
            let h = (items.len() as u16 + 2).min(full.height.saturating_sub(4));
            let area = centered(full, 70, h);
            let visible = h.saturating_sub(2) as usize;
            let start = cursor.saturating_sub(visible.saturating_sub(1));
            let lines = items
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .map(|(i, item)| {
                    let l = Line::raw(item.clone());
                    if i == *cursor { l.style(Style::new().reversed()) } else { l }
                })
                .collect();
            popup(f, area, &format!(" {title} "), lines);
        }
        Modal::Input { title, text, .. } => {
            let area = centered(full, 70, 3);
            popup(f, area, &format!(" {title} (Enter ok, Esc cancel) "), vec![Line::raw(format!("{text}█"))]);
        }
        Modal::ConfirmUndo { batch, count } => {
            let area = centered(full, 60, 3);
            popup(f, area, " undo ", vec![Line::raw(format!("Undo batch {batch} ({count} changes)? y/n"))]);
        }
        Modal::Review(st) => {
            let area = centered(full, 92, full.height.saturating_sub(2));
            let plan = &st.plan;
            let (n, bytes) = plan.totals();
            let mut lines = vec![Line::from(format!("Remove {n} files ({})", format_size(bytes))).bold()];
            for p in &plan.policies {
                let line = match &p.blocked {
                    Some(reason) => Line::from(format!("  {}: BLOCKED: {reason}", p.root.display())).fg(Color::Red),
                    None => {
                        let (cnt, b) = plan
                            .per_root()
                            .iter()
                            .find(|(pp, _, _)| pp.root == p.root)
                            .map(|(_, c, b)| (*c, *b))
                            .unwrap_or((0, 0));
                        let color = if p.kind == StrategyKind::Permanent { Color::Red } else { Color::Cyan };
                        Line::from(format!(
                            "  {}: {} — {cnt} files, {}",
                            p.root.display(),
                            p.explanation,
                            format_size(b)
                        ))
                        .fg(color)
                    }
                };
                lines.push(line);
            }
            lines.push(Line::raw(""));
            let header = lines.len() + 2;
            let visible = (area.height as usize).saturating_sub(header + 3);
            let start = st.cursor.saturating_sub(visible.saturating_sub(1));
            for (i, item) in plan.items.iter().enumerate().skip(start).take(visible) {
                let (mark, color, note) = if let Some(r) = &item.refused {
                    ("[!]", Color::Red, format!("  refused: {r}"))
                } else if item.only_copy && !item.include {
                    ("[ ]", Color::Yellow, "  ONLY COPY (not in a duplicate group); space to include".into())
                } else if item.only_copy {
                    ("[x]", Color::Red, "  ONLY COPY".into())
                } else if item.include {
                    ("[x]", Color::White, String::new())
                } else {
                    ("[ ]", Color::DarkGray, String::new())
                };
                let text = format!("{mark} {}  {}{note}", item.entry.rel, format_size(item.entry.size));
                let mut line = Line::from(text).fg(color);
                if i == st.cursor {
                    line = line.style(Style::new().reversed());
                }
                lines.push(line);
            }
            lines.push(Line::raw(""));
            lines.push(Line::from("space include/exclude · Enter confirm · Esc cancel").fg(Color::DarkGray));
            popup(f, area, " review ", lines);
        }
    }
}

fn popup(f: &mut Frame, area: Rect, title: &str, lines: Vec<Line>) {
    f.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(title.to_owned());
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
}
