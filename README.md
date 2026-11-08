# kinsift

Group files by their metadata, review the groups, act on them.

kinsift indexes a directory tree and records what each file says about
itself:
- the file system: path, size, times
- the file name: title, season, episode, year, resolution
- the contents: video streams, EXIF

You then look at your files through **views**, such as episodes by show,
season and episode, photos by capture date, or byte-identical files, and
clean up the groups that look wrong.

It is the successor of [mdupes](https://github.com/Flowm/mdupes), generalized
from "duplicate videos" to "any grouping by metadata".

> Status: v1 feature set, early days. Works:
> - indexing, the TUI with safe deletion
> - MKV/MP4 and EXIF metadata
> - staged content hashing
>
> See [docs/DECISIONS.md](docs/DECISIONS.md) for the design.

## What it reads

| Domain | Source | Examples |
|---|---|---|
| `fs` | the file system | `fs.path`, `fs.dir`, `fs.ext`, `fs.size`, `fs.mtime` |
| `name` | the path, parsed with [hunch](https://crates.io/crates/hunch) | `name.title`, `name.season`, `name.episode`, `name.year` |
| `video`, `audio` | MKV/WebM and MP4/MOV headers (built in, pure Rust); `ffprobe` for other containers if installed | `video.resolution`, `video.codec`, `video.hdr`, `video.fps`, `audio.languages` |
| `image`, `exif` | image headers and EXIF (built in); `exiftool` for CR3/RAF/RW2/ORF if installed | `exif.taken_at`, `exif.camera`, `image.pixels` |
| `content` | on demand: `kinsift hash` or `H` in the TUI | `content.hash` |

When the file name and the contents disagree, the contents win. The keys
that disagree are listed in `meta.conflicts`; see the `mislabeled` view.

## Build

`make help` lists the targets.

- `make build`: a native release binary with your Rust toolchain, at
  `target/release/kinsift`.
- `make build-docker`: a static Linux binary built in Docker, at
  `dist/kinsift`. No Rust toolchain needed. Cross-build with
  `PLATFORM=linux/arm64`.
- `make image`: the Docker image `kinsift:dev`.

### Docker

CI publishes the image to `ghcr.io/flowm/kinsift` for `main` and version
tags, for amd64 and arm64. The index lives in `/data` and the config in
`/config/config.toml`. Mount your libraries and run as your own user, so
kinsift acts on files with your permissions:

```sh
docker run --rm -it --user "$(id -u):$(id -g)" \
  -v ~/.local/share/kinsift-docker:/data \
  -v /mnt/series:/mnt/series \
  ghcr.io/flowm/kinsift scan /mnt/series
```

Use `tui` instead of `scan` for the TUI. Mount roots at the same path as on
the host, so paths in the index stay valid for both. The container's home is
`/data`. A root on the same file system as `/data` uses the trash there; any
other root uses quarantine. `kinsift roots` shows which one each root gets.
Read-only mounts are blocked. The image does not include `ffprobe` or
`exiftool`.

## Usage

Scan a few roots, then open the TUI:

```sh
kinsift scan /mnt/series /mnt/movies
kinsift tui --view series
kinsift
```

Running `kinsift` with no arguments opens the TUI too.

### The TUI

| Key | Action |
|---|---|
| `↑↓` `jk` `←→` `hl` `Enter` | move, expand/collapse |
| `e` `E` `c` `C` | expand subtree/everything, collapse subtree/everything |
| `v` | choose a view |
| `b` | group by other fields |
| `/` | filter |
| `f` | duplicate groups only |
| `s` | sort |
| `i` | details: every field and which provider set it |
| `m` | mark a file; on a group, mark by field value (e.g. all `720p`) |
| `K` | apply a keep-rule (e.g. *lower-resolution*) to the duplicate groups below the cursor |
| `D` | review and remove marked files |
| `d` | remove the file under the cursor |
| `r` | rename |
| `u` | undo the last batch |
| `R` | rescan in the background |
| `H` | hash this view's files that share a size (feeds the `by-hash` view) |
| `?` | help |

### Deleting safely

Nothing is removed without a review screen. That screen shows, per root,
what will happen:
- **OS trash**, if the root is on the same file system as your home
  directory
- **quarantine**, otherwise: a plain rename into `<root>/.kinsift-trash/`.
  It never copies across file systems.
- **permanent delete**, only if you configure it for a root, and only after
  typing `DELETE`

Before anything moves, kinsift checks the following:
- Each file must be unchanged since the scan.
- A group's last remaining copy is never removed.
- Files that are not in any duplicate group are flagged *only copy* and left
  out unless you include them.
- Roots that are read-only, unmounted, or on a different device than at the
  last scan are blocked.
- Files on a disk mounted inside a root are left alone. Add that disk as its
  own root.
- Roots may not overlap: scanning a folder inside (or around) a known root is
  refused, because the same file would be indexed twice.
- While a background rescan or hash runs, the TUI makes no changes.

A rescan never removes files from the index if some folders couldn't be
read. Files whose path is not valid UTF-8 are skipped and counted.

Every change is journaled; `u` in the TUI undoes the last batch.

```sh
kinsift roots
kinsift trash list
kinsift trash restore <batch>
kinsift trash purge --older-than 30
```

`kinsift roots` shows each root's delete strategy. `trash restore` moves a
quarantine batch back, in every root it touched. `trash purge` frees the space for good.

### From the command line

Rescan every known root:

```sh
kinsift scan
```

Find byte-identical files:

```sh
kinsift hash
kinsift query --view by-hash
```

Run views:

```sh
kinsift views
kinsift query --view series --duplicates
kinsift query --view movies --sort size
```

Ad-hoc grouping and filtering:

```sh
kinsift query -g name.title,name.year -f "name.type = movie and video.resolution >= 2160"
kinsift query -g fs.ext --sort count
```

Inspect one file, including which provider supplied each value:

```sh
kinsift inspect "/mnt/series/Show/Season 1/Show.S01E01.mkv" --raw
```

`kinsift fields` lists every field in the index.
`kinsift query --json` prints machine-readable output.

### Filters

Filters are field comparisons joined by `and`:

```
name.type = episode and video.resolution >= 1080 and fs.size > 2GiB
name.title ~ "star"          # case-insensitive contains
exif.taken_at is not null
exif.taken_at >= 2024-01-01   # dates, or "2024-01-01 12:30" (UTC)
```

### Configuration

`~/.config/kinsift/config.toml` uses the same format as the built-in
defaults. Print them with `kinsift config dump-defaults`. It defines:
- views
- keep-rules
- provider priority
- per-root delete strategy

The index lives at `~/.local/share/kinsift/index.db`. Override the locations
with `--index` / `--config` or `KINSIFT_INDEX` / `KINSIFT_CONFIG`.

## Development

```sh
make test
make lint
```

Two opt-in accuracy checks for the file name parser:

```sh
# guessit's fixtures (LGPL, downloaded into target/, never committed)
scripts/fetch-guessit-fixtures.sh
cargo test -p kinsift-providers --test guessit_fixtures -- --ignored --nocapture

# your own library, kept outside the repository; prints aggregates only
mkdir -p ~/.local/share/kinsift/corpus
find /path/to/library -type f > ~/.local/share/kinsift/corpus/library.txt
cargo test -p kinsift-providers --test corpus -- --ignored --nocapture
```

Never commit real file names. Test fixtures in this repository are synthetic.

## License

MIT
