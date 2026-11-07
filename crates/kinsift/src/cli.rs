use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use kinsift_core::config::{self, Config, DEFAULT_CONFIG, View};
use kinsift_core::filter::Filter;
use kinsift_core::index::{FileEntry, Index};
use kinsift_core::provider::{Priority, Provider};
use kinsift_core::query::{Node, SortBy, build_tree, sort_tree};
use kinsift_core::scan::{ScanEvent, ScanOptions, scan_root};
use kinsift_core::value::{Value, format_size, format_unix};
use serde_json::json;

#[derive(Parser, Debug)]
#[command(name = "kinsift", version, about = "Group files by metadata, review the groups, act on them.")]
pub struct Args {
    /// Index database (default: ~/.local/share/kinsift/index.db, or $KINSIFT_INDEX)
    #[arg(long, global = true, value_name = "PATH")]
    pub index: Option<PathBuf>,
    /// Config file (default: ~/.config/kinsift/config.toml, or $KINSIFT_CONFIG)
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Scan roots and update the index (all known roots if none are given)
    Scan {
        roots: Vec<PathBuf>,
        /// Prune vanished files even when the mount guard refuses
        #[arg(long)]
        force_prune: bool,
    },
    /// Group indexed files and print the tree
    Query {
        /// Use a saved view (see `kinsift views`)
        #[arg(long, short)]
        view: Option<String>,
        /// Comma-separated group-by fields, e.g. name.title,name.season
        #[arg(long, short = 'g', value_delimiter = ',')]
        group_by: Vec<String>,
        /// Filter, e.g. "name.type = movie and video.resolution >= 2160"
        #[arg(long, short)]
        filter: Option<String>,
        /// Only show duplicate groups
        #[arg(long, short)]
        duplicates: bool,
        /// Sort groups by: name, size, count, dups
        #[arg(long, default_value = "name")]
        sort: String,
        /// Extra fields to show per file (comma-separated)
        #[arg(long, value_delimiter = ',')]
        fields: Vec<String>,
        /// Print JSON instead of a tree
        #[arg(long)]
        json: bool,
    },
    /// Show everything known about one file
    Inspect {
        file: PathBuf,
        /// Also print each provider's raw output
        #[arg(long)]
        raw: bool,
        #[arg(long)]
        json: bool,
    },
    /// List or remove scan roots
    Roots {
        #[command(subcommand)]
        action: Option<RootsAction>,
    },
    /// List fields present in the index with file counts
    Fields,
    /// List saved views
    Views,
    /// Configuration helpers
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum RootsAction {
    /// Forget a root and its files (nothing on disk is touched)
    Remove { path: PathBuf },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the built-in defaults (a starting point for your config)
    DumpDefaults,
    /// Print the config and index paths in use
    Paths,
}

/// Everything a command needs.
pub struct Context {
    pub config: Config,
    pub config_path: Option<PathBuf>,
    pub index_path: PathBuf,
    pub index: Index,
    pub providers: Vec<Box<dyn Provider>>,
    pub priority: Priority,
}

impl Context {
    pub fn open(args: &Args) -> Result<Context> {
        let config_path = args.config.clone().or_else(config::default_config_path);
        let config = Config::load(config_path.as_deref())?;
        let index_path = args
            .index
            .clone()
            .or_else(config::default_index_path)
            .ok_or_else(|| anyhow!("cannot determine the index location; pass --index"))?;
        let mut index = Index::open(&index_path).with_context(|| format!("opening {}", index_path.display()))?;
        let providers = kinsift_providers::default_providers();
        let priority = Priority::new(config.providers.priority.clone(), &providers);
        kinsift_core::sync_priority(&mut index, &config, &priority)?;
        Ok(Context { config, config_path, index_path, index, providers, priority })
    }
}

pub fn run(args: Args) -> Result<()> {
    let Some(command) = &args.command else {
        bail!("no command given; try `kinsift --help` (the TUI arrives in a later milestone)");
    };
    if let Command::Config { action: ConfigAction::DumpDefaults } = command {
        print!("{DEFAULT_CONFIG}");
        return Ok(());
    }
    let mut ctx = Context::open(&args)?;
    match command {
        Command::Scan { roots, force_prune } => cmd_scan(&mut ctx, roots, *force_prune),
        Command::Query { view, group_by, filter, duplicates, sort, fields, json } => {
            let view = resolve_view(&ctx.config, view.as_deref(), group_by, filter.as_deref(), *duplicates)?;
            cmd_query(&ctx, &view, parse_sort(sort)?, fields, *json)
        }
        Command::Inspect { file, raw, json } => cmd_inspect(&ctx, file, *raw, *json),
        Command::Roots { action } => cmd_roots(&ctx, action.as_ref()),
        Command::Fields => {
            for (key, count) in ctx.index.field_counts()? {
                println!("{count:>8}  {key}");
            }
            Ok(())
        }
        Command::Views => {
            for v in &ctx.config.views {
                println!("{:<18} {}", v.name, v.description);
            }
            Ok(())
        }
        Command::Config { action: ConfigAction::Paths } => {
            let cfg = ctx.config_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(none)".into());
            println!("config: {cfg}");
            println!("index:  {}", ctx.index_path.display());
            Ok(())
        }
        Command::Config { action: ConfigAction::DumpDefaults } => unreachable!(),
    }
}

/// Roots to scan when none are given: known roots plus configured ones.
pub fn known_roots(ctx: &Context) -> Result<Vec<PathBuf>> {
    let mut roots: Vec<PathBuf> = ctx.index.roots()?.into_iter().map(|r| r.path).collect();
    for r in &ctx.config.roots {
        let p = r.path.canonicalize().unwrap_or_else(|_| r.path.clone());
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    Ok(roots)
}

fn cmd_scan(ctx: &mut Context, roots: &[PathBuf], force_prune: bool) -> Result<()> {
    let roots = if roots.is_empty() { known_roots(ctx)? } else { roots.to_vec() };
    if roots.is_empty() {
        bail!("no roots known yet; run `kinsift scan <dir>...`");
    }
    let tty = std::io::stderr().is_terminal();
    let progress = |ev: ScanEvent| {
        if !tty {
            return;
        }
        let line = match ev {
            ScanEvent::Walking { root, found } => format!("walking {}: {found} files", root.display()),
            ScanEvent::Extracting { root, done, total } => format!("reading {}: {done}/{total}", root.display()),
            ScanEvent::Done(_) => String::new(),
        };
        eprint!("\r\x1b[2K{line}");
        let _ = std::io::stderr().flush();
    };
    let opts = ScanOptions { force_prune };
    let mut failed = false;
    for root in roots {
        match scan_root(&mut ctx.index, &ctx.providers, &ctx.priority, &root, &opts, &progress) {
            Ok(r) => {
                if tty {
                    eprint!("\r\x1b[2K");
                }
                println!(
                    "{}: {} files ({} new, {} changed, {} read, {} errors, {} pruned)",
                    r.root.display(),
                    r.found,
                    r.new,
                    r.changed,
                    r.extracted,
                    r.errors,
                    r.pruned
                );
                if let Some(reason) = r.prune_skipped {
                    println!("  kept vanished files in the index: {reason}");
                    println!("  (rescan when the root is back, or use --force-prune if they are really gone)");
                }
                if r.non_utf8 > 0 {
                    println!("  {} files skipped: their path is not valid UTF-8 (see RUST_LOG=warn)", r.non_utf8);
                }
                if r.walk_errors > 0 {
                    println!("  {} entries could not be read (see RUST_LOG=warn)", r.walk_errors);
                }
            }
            Err(e) => {
                if tty {
                    eprint!("\r\x1b[2K");
                }
                eprintln!("{}: {e}", root.display());
                failed = true;
            }
        }
    }
    if failed {
        bail!("some roots could not be scanned");
    }
    Ok(())
}

pub fn resolve_view(
    config: &Config,
    name: Option<&str>,
    group_by: &[String],
    filter: Option<&str>,
    duplicates: bool,
) -> Result<View> {
    let mut view = match name {
        Some(n) => config.view(n).cloned().ok_or_else(|| anyhow!("no view named `{n}`; see `kinsift views`"))?,
        None => View { name: "query".into(), group_by: vec!["fs.root".into(), "fs.path".into()], ..Default::default() },
    };
    if !group_by.is_empty() {
        view.group_by = group_by.to_vec();
    }
    if let Some(f) = filter {
        view.filter = Some(match &view.filter {
            Some(existing) => format!("{existing} and {f}"),
            None => f.to_owned(),
        });
    }
    view.duplicates_only |= duplicates;
    Ok(view)
}

pub fn load_view(index: &Index, view: &View) -> Result<(Vec<FileEntry>, Node)> {
    let filter = view.filter.as_deref().map(Filter::parse).transpose()?;
    let entries = index.load_entries(filter.as_ref(), None)?;
    let tree = build_tree(&entries, &view.group_by, view.duplicates_only);
    Ok((entries, tree))
}

fn parse_sort(s: &str) -> Result<SortBy> {
    Ok(match s {
        "name" => SortBy::Name,
        "size" => SortBy::Size,
        "count" => SortBy::Count,
        "dups" | "duplicates" => SortBy::Duplicates,
        other => bail!("unknown sort `{other}` (name, size, count, dups)"),
    })
}

fn cmd_query(ctx: &Context, view: &View, sort: SortBy, fields: &[String], json: bool) -> Result<()> {
    let (entries, mut tree) = load_view(&ctx.index, view)?;
    sort_tree(&mut tree, sort, &entries);
    if json {
        println!("{}", serde_json::to_string_pretty(&node_json(&tree, &entries))?);
        return Ok(());
    }
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "{}: {} files, {}, {} duplicate groups",
        view.name,
        tree.file_count,
        format_size(tree.size),
        tree.dup_groups
    )?;
    print_node(&mut out, &tree, &entries, fields, 0)?;
    Ok(())
}

fn print_node(out: &mut impl Write, node: &Node, entries: &[FileEntry], fields: &[String], depth: usize) -> Result<()> {
    let pad = "  ".repeat(depth);
    for &i in &node.files {
        writeln!(out, "{pad}{}", file_line(&entries[i], fields))?;
    }
    for child in &node.children {
        let dup = if child.duplicate { "  [duplicates]" } else { "" };
        writeln!(out, "{pad}{}  ({} files, {}){dup}", child.label, child.file_count, format_size(child.size))?;
        print_node(out, child, entries, fields, depth + 1)?;
    }
    Ok(())
}

/// One line per file: relative path plus a short metadata summary.
pub fn file_line(e: &FileEntry, extra: &[String]) -> String {
    let mut parts = vec![e.rel.clone()];
    if let Some(r) = e.get("video.resolution") {
        parts.push(format!("{r}p"));
    }
    if let Some(c) = e.get("video.codec") {
        parts.push(c.to_string());
    }
    for key in extra {
        if let Some(v) = e.get(key) {
            parts.push(format!("{key}={v}"));
        }
    }
    parts.push(format_size(e.size));
    parts.join("  ")
}

fn node_json(node: &Node, entries: &[FileEntry]) -> serde_json::Value {
    json!({
        "label": node.label,
        "field": node.field,
        "files": node.file_count,
        "size": node.size,
        "duplicate": node.duplicate,
        "duplicate_groups": node.dup_groups,
        "entries": node.files.iter().map(|&i| entry_json(&entries[i])).collect::<Vec<_>>(),
        "groups": node.children.iter().map(|c| node_json(c, entries)).collect::<Vec<_>>(),
    })
}

fn entry_json(e: &FileEntry) -> serde_json::Value {
    json!({
        "path": e.abs(),
        "size": e.size,
        "fields": e.fields.iter().map(|(k, v)| (k.clone(), value_json(v))).collect::<serde_json::Map<_, _>>(),
    })
}

fn value_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Str(s) => json!(s),
        Value::Int(i) => json!(i),
        Value::Float(f) | Value::Duration(f) => json!(f),
        Value::Time(t) => json!(format_unix(*t)),
        Value::Path(p) => json!(p.join("/")),
        Value::List(items) => serde_json::Value::Array(items.iter().map(value_json).collect()),
    }
}

fn cmd_inspect(ctx: &Context, file: &Path, raw: bool, json: bool) -> Result<()> {
    let entry = ctx
        .index
        .find_by_path(file)?
        .ok_or_else(|| anyhow!("{} is not in the index (scan its root first)", file.display()))?;
    let sources = ctx.index.sources(entry.id)?;
    let extractions = ctx.index.extractions(entry.id)?;
    if json {
        let out = json!({
            "path": entry.abs(),
            "size": entry.size,
            "fields": entry.fields.iter().map(|(k, v)| (k.clone(), value_json(v))).collect::<serde_json::Map<_, _>>(),
            "sources": sources.iter().map(|s| json!({
                "key": s.key, "provider": s.provider, "value": value_json(&s.value), "effective": s.effective,
            })).collect::<Vec<_>>(),
            "providers": extractions.iter().map(|x| json!({
                "provider": x.provider,
                "version": x.version,
                "error": x.error,
                "raw": x.raw.as_deref().and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok()),
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!("{}", entry.abs().display());
    println!();
    let width = entry.fields.keys().map(String::len).max().unwrap_or(10);
    for (key, value) in &entry.fields {
        let winners: Vec<&str> =
            sources.iter().filter(|s| &s.key == key && s.effective).map(|s| s.provider.as_str()).collect();
        let others: Vec<String> = sources
            .iter()
            .filter(|s| &s.key == key && !s.effective)
            .map(|s| format!("{}: {}", s.provider, s.value))
            .collect();
        let from = winners.first().copied().unwrap_or("kinsift");
        let rest = if others.is_empty() { String::new() } else { format!("   (also {})", others.join("; ")) };
        println!("  {key:<width$}  {value}   [{from}]{rest}");
    }
    println!();
    for x in &extractions {
        match &x.error {
            Some(err) => println!("  provider {} v{}: error: {err}", x.provider, x.version),
            None => println!("  provider {} v{}: ok", x.provider, x.version),
        }
        if raw && let Some(r) = &x.raw {
            let pretty = serde_json::from_str::<serde_json::Value>(r)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or_else(|| r.clone());
            for line in pretty.lines() {
                println!("      {line}");
            }
        }
    }
    Ok(())
}

fn cmd_roots(ctx: &Context, action: Option<&RootsAction>) -> Result<()> {
    match action {
        None => {
            for r in ctx.index.roots()? {
                let when = r.last_scan.map(format_unix).unwrap_or_else(|| "never".into());
                println!("{}  ({} files, last scan {when})", r.path.display(), r.file_count);
            }
        }
        Some(RootsAction::Remove { path }) => {
            let p = path.canonicalize().unwrap_or_else(|_| path.clone());
            if ctx.index.remove_root(&p)? {
                println!("forgot {} (files on disk untouched)", p.display());
            } else {
                bail!("{} is not a known root", p.display());
            }
        }
    }
    Ok(())
}
