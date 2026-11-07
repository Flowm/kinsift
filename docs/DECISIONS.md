# kinsift: decision record

The design was settled in a structured interview before the first line of
code (decisions Q1–Q56). Q numbers are kept so later changes can refer to
them. Where the implementation refined a decision, the note says so.

## Product

- **Name:** kinsift. MIT license. Replaces
  [mdupes](https://github.com/Flowm/mdupes), which is archived at v1. (Q12, Q23, Q31)
- **What it is:** a tool that groups files by metadata so you can review
  the groups and act on them. It is not a general file manager. (Q4)
- **Audience:** personal first, public on GitHub, distributable. (Q5)
- **v1 covers:** video (everything mdupes does) plus photo duplicates. A
  content hash works as a grouping key for any file type. (Q6, Q13)
- **What it changes on disk:** metadata is never written. Each root gets a
  delete strategy: the OS trash if the root is on the home filesystem,
  otherwise kinsift's quarantine. Permanent delete happens only when
  configured per root, with a typed confirmation. Rename is supported. Move
  and template rename come after v1. (Q10, Q11, Q43, Q50)

## Technology

- **Rust.** Embedding guessit in a static Go binary would need an immature
  CPython-on-WASI setup. hunch, measured on a sample library, agreed with
  guessit on the grouping key for 98.4% of episodes and 92.2% of movies, and
  most disagreements were guessit's mistakes. (Q15, Q25, Q26)
- **Cargo workspace:**
  - `kinsift-core`: data model, index, queries, file actions. No UI code.
  - `kinsift-providers`: one module per provider, each behind a cargo feature.
  - `kinsift-tui`: ratatui, structured the Elm way (state, messages, update, view).
  - `kinsift`: the binary. CLI subcommands, with `tui` as the default.
  - `kinsift-gui`: later. (Q9, Q34, Q37)
- **Builds:** a static musl TUI binary. The GUI is a separate build target
  later, with its toolkit chosen then. (Q14)
- **Concurrency:** a rayon thread pool, channels to the TUI, no tokio. (Q35)
- **Index:** SQLite in WAL mode, one central index at
  `~/.local/share/kinsift/index.db`. Tables:
  - `files`
  - `field_values`: every provider's value
  - `fields`: the winning value per key, typed by column
  - `extractions`: raw provider output, provider version, errors
  - `actions`: the journal

  A portable per-root index comes later. DuckDB can query the index from
  outside with `ATTACH`. (Q8/Q24, Q22, Q36)
- **Config:** `~/.config/kinsift/config.toml` with `[[roots]]`,
  `[providers.priority]`, `[[views]]`, `[[keep_rules]]` and `[trash]`. The
  built-in defaults use the same format, and
  `kinsift config dump-defaults` prints them. (Q21, Q44)

## Metadata model

- **Two layers:**
  - Each provider produces its own raw output.
  - Code in the provider maps that output into domain structs (`fs`, `name`,
    `video`, `audio`, `image`, `exif`, `content`), which are flattened into
    typed fields such as `video.height`.
  - Every value records which provider set it. A configurable priority
    decides conflicts.
  - Keys where providers disagree are listed in `meta.conflicts`. (Q7, Q18, Q29)
- **Value types:** string, int, float, time, duration, path (hierarchical),
  list. (Q7, Q28)
- **Raw output** is kept in the index. You see it in the TUI detail pane and
  with `kinsift inspect <file> [--raw] [--json]`. (Q30)
- **Providers:** compiled in behind a trait. There is no guessit adapter and
  no external-provider protocol until a real need appears. Readers written in
  Rust come first. ffprobe and exiftool are optional built-in providers when
  they are on PATH. (Q17, Q19, Q27, Q38)
- **The name provider:**
  - hunch is a pinned crates.io dependency.
  - Localized season folders (`Staffel`, `Saison`, `Temporada`, ...) are
    normalized before parsing.
  - It retries with hunch's sibling-file context mode when confidence is low
    or the title looks like a season folder.
  - It falls back to the show folder for `<Show>/<Season N>/<file>` layouts.
  - Fixes go upstream; fork only if upstream stalls. (Q16, Q31, Q33)
- **Resolution from the file name:** the name provider fills
  `video.resolution` and `video.codec` at low priority. Container readers fill
  the same fields at higher priority. (Q48)
  - *Refinement:* the shared field is `video.resolution`, a resolution class
    (2160, 1080, 720, ...) derived from width and height, not
    `video.height`. Otherwise 1920×800 scope encodes would look like a
    conflict with their `1080p` name.

## Scanning

- **Staged pipeline:**
  1. Walk the root.
  2. Parse names.
  3. Read container and EXIF headers.
  4. Hash only on demand: among files of equal size, a partial blake3 hash
     (first and last 64 KiB), then a full blake3 hash. (Q20, Q39)
- **Rescans** re-extract a file when `(path, size, mtime_ns)` changes; inodes
  are ignored. Each provider's version is stored per file, so upgrading one
  provider re-runs only that provider. (Q40)
- **Pruning** removes vanished files, **except** when one of these holds; the
  scan then keeps them and warns:
  - the root's device ID changed,
  - the root is empty,
  - more than 20% of its files vanished at once. (Q40, Q54)

## Views, duplicates, actions

- **A view** is an ordered list of group-by fields, plus a filter and
  `duplicates_only`. `field:year`, `field:month` and `field:day` bucket time
  fields. You can also group ad hoc in the TUI. Built-in views:
  - `folders` (the default)
  - `series`
  - `movies`
  - `same-folder`
  - `mislabeled`
  - `photos-by-date`
  - `photo-duplicates`
  - `by-hash`

  (Q21, Q28, Q48)
- **A duplicate group** is a last-level group with more than one file. No
  perceptual hashing in v1. (Q20)
- **Filters:** field comparisons joined by `and`, compiled to SQL. (Q41)
- **Keep-rules** rank the files inside each group. Two modes:
  - `keep-best`: keep one file per group.
  - `drop-dominated`: mark only files strictly worse on the first term, and
    keep every file tied at the top.

  The `lower-resolution` preset uses `drop-dominated` on
  `video.resolution`. (Q42, Q47)
- **Manual mark-by-value** works on any field. Every bulk action goes
  through the review screen. (Q49)

## Deletion safety

- **Strategy per root, never a silent fallback:** the OS trash is used only
  after confirming the file is on the home device. That rules out the trash
  library's copy-to-home-trash fallback. (Q50)
- **Quarantine:**
  - A plain `rename` into `<root>/.kinsift-trash/<batch>/<relative path>`.
    It fails on a cross-device move instead of copying, and never replaces
    an existing path.
  - A configured quarantine directory may be shared by several roots. There
    the batch folder is `<batch>-<root tag>`, one per root, so each root has
    its own manifest and equal relative paths cannot collide.
  - Each batch has a `manifest.json`.
  - Commands: `kinsift trash list | restore | purge`.
  - The TUI shows quarantine size per root and reminds you past a configured
    size or age. Nothing is purged automatically. (Q51)
- **Re-checked right before acting:**
  - `lstat` must still match the size and mtime in the index.
  - Hash duplicates need a verified full hash.
  - Read-only roots are disabled up front.
  - Errors are reported file by file. (Q52)
- **Never the last copy:** `kinsift-core` checks every plan so each group
  keeps at least one existing member. Files that aren't in any duplicate
  group are flagged "only copy" and excluded unless you opt in. (Q53)
- **Unmounted mounts:** no deletes on a root whose device ID changed since the
  scan. (Q54)
- **Journal and undo:** an `actions` table records every change. The TUI has
  "Undo last batch". (Q55)
- **No file changes from the CLI in v1:** delete and rename are TUI-only. The
  CLI's only changes are `trash restore/purge`, for recovery. (Q56)

## Testing, privacy, CI

- **Tests in the repo** use only synthetic file names. guessit's fixtures
  (LGPL) are downloaded at test time by
  `scripts/fetch-guessit-fixtures.sh` and never committed. (Q16, Q32)
- **Private corpus:** real file names live in a corpus outside the repo
  (`~/.local/share/kinsift/corpus/*.txt`). An ignored test,
  `cargo test -p kinsift-providers --test corpus -- --ignored --nocapture`,
  runs against it and prints only aggregate numbers. (Q32)
- **CI:** fmt, clippy and tests on Linux, macOS and Windows on every push.
  Tagged releases build static musl Linux binaries (amd64/arm64) plus macOS and
  Windows. crates.io and Homebrew come after v1. (Q46)

## Milestones

Status: M1 and M2 are done.

- **M1, the pipeline:**
  - workspace
  - fs and hunch providers
  - index with incremental rescans and the prune guard
  - `scan`, `query`, `inspect`
  - corpus test
- **M2, mdupes parity:**
  - TUI with views and presets, the folder tree, the inspect pane
  - marking and keep-rules
  - review step, delete strategies and quarantine
  - journal and undo, `trash` subcommands
- **M3, v1:**
  - MP4 and MKV providers, EXIF
  - staged hashing
  - photo presets

  mdupes gets archived after M3. (Q45)

## Deferred until after v1

- GUI
- move and template rename
- perceptual hashing
- portable per-root index
- external-provider protocol
- `or` and parentheses in filters
- crates.io and Homebrew releases
