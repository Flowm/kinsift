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

> Status: early development. Milestone 1 (index, scanning, CLI queries)
> works. The TUI, deletion and container metadata are next. See
> [docs/DECISIONS.md](docs/DECISIONS.md) for the design.

## Build

```sh
cargo build --release
./target/release/kinsift --help
```

## Usage

Scan a few roots, then rescan them later:

```sh
kinsift scan /mnt/series /mnt/movies
kinsift scan
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
cargo test --all
cargo clippy --all-targets -- -D warnings
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
