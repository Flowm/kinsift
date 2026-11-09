//! Media metadata parsed from the file path, using `hunch`.
//!
//! On top of hunch this provider:
//! - rewrites localized season folders (`Staffel 3`, `Saison 2`, ...) to
//!   `Season N` before parsing,
//! - retries with sibling file names as context when hunch is unsure or the
//!   title looks like a season folder (hunch's context mode),
//! - falls back to the show folder for `<Show>/<Season N>/<file>` layouts,
//! - fills `video.resolution` and `video.codec` at low priority, so
//!   resolution-based rules work before container readers have run.

use std::path::Path;
use std::sync::LazyLock;

use hunch::{Confidence, HunchResult, MediaType, Property};
use kinsift_core::domain::{Domain, MediaMeta, ReleaseMeta, VideoMeta};
use kinsift_core::provider::{Extraction, FileCtx, Provider, Stage, VIDEO_EXTENSIONS};
use regex::Regex;

pub struct NameProvider;

/// Season folder names in several languages, plus "Specials".
static SEASON_DIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:season|series|staffel|saison|temporada|stagione|seizoen|sezon|säsong|sæson|sesong|kausi)[ ._-]*(\d{1,3})$")
        .expect("valid regex")
});
static SPECIALS_DIR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:specials?|extras?|season[ ._-]*0+)$").expect("valid regex"));
static TRAILING_YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?)[ ._]*[(\[]((?:19|20)\d{2})[)\]]\s*$").expect("valid regex"));

const MAX_SIBLINGS: usize = 200;

impl Provider for NameProvider {
    fn name(&self) -> &'static str {
        "name"
    }

    fn version(&self) -> u32 {
        1
    }

    fn stage(&self) -> Stage {
        Stage::Name
    }

    /// Low: anything read from the file contents beats the file name.
    fn default_priority(&self) -> i32 {
        10
    }

    fn applies(&self, ctx: &FileCtx) -> bool {
        ctx.is_video()
    }

    fn extract(&self, ctx: &FileCtx) -> Result<Option<Extraction>, String> {
        Ok(Some(parse(ctx.abs)))
    }
}

/// Parse one path. Public for tests and tools.
pub fn parse(abs: &Path) -> Extraction {
    let input = normalize_season_dirs(&abs.to_string_lossy());
    let mut result = hunch::hunch(&input);
    let mut used_context = false;
    if needs_context(&result) {
        let siblings = sibling_names(abs);
        if !siblings.is_empty() {
            result = hunch::hunch_with_context(&input, &siblings);
            used_context = true;
        }
    }

    let mut title = result.title().map(str::to_owned);
    let mut year = result.year().map(i64::from);
    let mut title_from_folder = false;
    if (title.as_deref().is_none_or(is_season_like))
        && let Some((show, show_year)) = show_from_folders(abs)
    {
        title = Some(show);
        year = year.or(show_year);
        title_from_folder = true;
    }

    let mut episodes: Vec<i64> = result.all(Property::Episode).iter().filter_map(|e| e.parse().ok()).collect();
    episodes.dedup();
    let kind = match result.media_type() {
        Some(MediaType::Episode) => Some("episode"),
        Some(MediaType::Movie) => Some("movie"),
        Some(MediaType::Extra) => Some("extra"),
        _ => None,
    };
    let confidence = match result.confidence() {
        Confidence::Low => "low",
        Confidence::Medium => "medium",
        _ => "high",
    };

    let media = MediaMeta {
        r#type: kind.map(str::to_owned),
        title,
        year,
        season: result.season().map(i64::from),
        episode: (!episodes.is_empty()).then_some(episodes),
        episode_title: result.episode_title().map(str::to_owned),
        edition: result.edition().map(str::to_owned),
        date: result.date().map(str::to_owned),
        confidence: Some(confidence.to_owned()),
    };
    let release = ReleaseMeta {
        group: result.release_group().map(str::to_owned),
        source: result.source().map(str::to_owned),
        screen_size: result.screen_size().map(str::to_owned),
        video_codec: result.video_codec().map(str::to_owned),
        audio_codec: result.audio_codec().map(str::to_owned),
    };
    let video = VideoMeta {
        resolution: result.screen_size().and_then(resolution_from_tag),
        codec: result.video_codec().and_then(normalize_codec),
        ..Default::default()
    };

    let mut raw = serde_json::to_value(result.to_flat_map()).unwrap_or_default();
    if let Some(obj) = raw.as_object_mut() {
        obj.insert("_input".into(), input.into());
        obj.insert("_context".into(), used_context.into());
        obj.insert("_title_from_folder".into(), title_from_folder.into());
    }
    Extraction { sets: vec![media.into_fields(), release.into_fields(), video.into_fields()], raw: Some(raw) }
}

fn needs_context(r: &HunchResult) -> bool {
    r.confidence() == Confidence::Low || r.title().is_none_or(is_season_like)
}

fn is_season_like(title: &str) -> bool {
    let t = title.trim();
    SEASON_DIR.is_match(t) || SPECIALS_DIR.is_match(t)
}

/// Rewrite localized season directory names to `Season N`.
pub fn normalize_season_dirs(path: &str) -> String {
    let mut parts: Vec<String> = path.split('/').map(str::to_owned).collect();
    let last = parts.len().saturating_sub(1);
    for part in &mut parts[..last] {
        if let Some(c) = SEASON_DIR.captures(part) {
            *part = format!("Season {}", &c[1]);
        }
    }
    parts.join("/")
}

/// For `<Show>/<Season N or Specials>/<file>`, return the show folder name
/// (without a trailing `(year)`) and that year.
fn show_from_folders(abs: &Path) -> Option<(String, Option<i64>)> {
    let parent = abs.parent()?;
    let parent_name = parent.file_name()?.to_string_lossy();
    if !(SEASON_DIR.is_match(&parent_name) || SPECIALS_DIR.is_match(&parent_name)) {
        return None;
    }
    let show = parent.parent()?.file_name()?.to_string_lossy().into_owned();
    match TRAILING_YEAR.captures(&show) {
        Some(c) => Some((c[1].trim().to_owned(), c[2].parse().ok())),
        None => Some((show, None)),
    }
}

fn sibling_names(abs: &Path) -> Vec<String> {
    let Some(dir) = abs.parent() else { return Vec::new() };
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    read.filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            Path::new(n)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        })
        .filter(|n| Some(n.as_str()) != abs.file_name().and_then(|f| f.to_str()))
        .take(MAX_SIBLINGS)
        .collect()
}

/// `1080p` → 1080, `4K`/`2160p` → 2160, `720i` → 720.
pub fn resolution_from_tag(tag: &str) -> Option<i64> {
    let t = tag.trim().to_ascii_lowercase();
    match t.as_str() {
        "4k" | "uhd" => return Some(2160),
        "8k" => return Some(4320),
        _ => {}
    }
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    let n: i64 = digits.parse().ok()?;
    // "1920x1080" style tags
    if let Some((_, h)) = t.split_once('x') {
        return h.trim_end_matches(['p', 'i']).parse().ok();
    }
    Some(n)
}

/// Map codec spellings to one vocabulary: hevc, h264, av1, vp9, mpeg4, mpeg2, vc1.
pub fn normalize_codec(codec: &str) -> Option<String> {
    let c = codec.to_ascii_lowercase().replace(['.', '-', ' ', '_'], "");
    let norm = match c.as_str() {
        "h265" | "hevc" | "x265" => "hevc",
        "h264" | "avc" | "x264" | "avc1" => "h264",
        "av1" => "av1",
        "vp9" => "vp9",
        "vp8" => "vp8",
        "xvid" | "divx" | "mpeg4" | "mpeg4visual" | "mp4v" => "mpeg4",
        "mpeg2" | "h262" => "mpeg2",
        "vc1" => "vc1",
        "" => return None,
        _ => return Some(c),
    };
    Some(norm.to_owned())
}

#[cfg(test)]
mod tests {
    //! Synthetic names only: never put real library file names here.
    use super::*;
    use kinsift_core::value::Value;
    use std::path::PathBuf;

    fn field(e: &Extraction, key: &str) -> Option<Value> {
        e.sets.iter().flat_map(|s| s.keys().map(|(k, v)| (k, v.clone()))).find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn s(e: &Extraction, key: &str) -> Option<String> {
        field(e, key).map(|v| v.to_string())
    }

    #[test]
    fn plain_episode() {
        let e = parse(&PathBuf::from("/lib/tv/Northwind Tales/Season 2/Northwind.Tales.S02E05.1080p.WEB.x265-GRP.mkv"));
        assert_eq!(s(&e, "media.title").as_deref(), Some("Northwind Tales"));
        assert_eq!(field(&e, "media.season"), Some(Value::Int(2)));
        assert_eq!(field(&e, "media.episode"), Some(Value::Int(5)));
        assert_eq!(s(&e, "media.type").as_deref(), Some("episode"));
        assert_eq!(field(&e, "video.resolution"), Some(Value::Int(1080)));
        assert_eq!(s(&e, "video.codec").as_deref(), Some("hevc"));
    }

    #[test]
    fn multi_episode() {
        let e = parse(&PathBuf::from("/lib/tv/Quill Harbor/Season 1/Quill.Harbor.S01E01E02.720p.HDTV.x264.mkv"));
        assert_eq!(field(&e, "media.episode"), Some(Value::List(vec![Value::Int(1), Value::Int(2)])));
    }

    #[test]
    fn localized_season_folder_with_bare_file_name() {
        let e = parse(&PathBuf::from("/lib/tv/Copper Valley/Staffel 3/S03E10.avi"));
        assert_eq!(s(&e, "media.title").as_deref(), Some("Copper Valley"));
        assert_eq!(field(&e, "media.season"), Some(Value::Int(3)));
        assert_eq!(field(&e, "media.episode"), Some(Value::Int(10)));
    }

    #[test]
    fn specials_folder_uses_show_name() {
        let e = parse(&PathBuf::from("/lib/tv/Harbor Lights/Specials/S00E04 - Behind the Fog.mp4"));
        assert_eq!(s(&e, "media.title").as_deref(), Some("Harbor Lights"));
    }

    #[test]
    fn title_with_dash_stays_whole() {
        let e = parse(&PathBuf::from(
            "/lib/tv/Lantern Bay- The Long Night/Season 1/Lantern.Bay-.The.Long.Night.S01E03.WEBDL-1080p.h264.mkv",
        ));
        let title = s(&e, "media.title").unwrap();
        assert!(title.contains("Lantern Bay") && title.contains("Long Night"), "got {title}");
    }

    #[test]
    fn movie_with_folder_year() {
        let e = parse(&PathBuf::from("/lib/movies/Paper Comets (2019)/Paper.Comets.2019.Bluray-2160p.x265.mkv"));
        assert_eq!(s(&e, "media.title").as_deref(), Some("Paper Comets"));
        assert_eq!(field(&e, "media.year"), Some(Value::Int(2019)));
        assert_eq!(s(&e, "media.type").as_deref(), Some("movie"));
        assert_eq!(field(&e, "video.resolution"), Some(Value::Int(2160)));
    }

    #[test]
    fn season_dir_normalization() {
        assert_eq!(normalize_season_dirs("/a/Show/Staffel 3/S03E01.avi"), "/a/Show/Season 3/S03E01.avi");
        assert_eq!(normalize_season_dirs("/a/Show/Saison_02/x.mkv"), "/a/Show/Season 02/x.mkv");
        assert_eq!(normalize_season_dirs("/a/Staffel 3.mkv"), "/a/Staffel 3.mkv", "file names are left alone");
    }

    #[test]
    fn resolution_tags() {
        assert_eq!(resolution_from_tag("1080p"), Some(1080));
        assert_eq!(resolution_from_tag("4K"), Some(2160));
        assert_eq!(resolution_from_tag("1920x1080"), Some(1080));
        assert_eq!(resolution_from_tag("576i"), Some(576));
        assert_eq!(resolution_from_tag("HD"), None);
    }

    #[test]
    fn codecs() {
        assert_eq!(normalize_codec("H.265").as_deref(), Some("hevc"));
        assert_eq!(normalize_codec("AVC").as_deref(), Some("h264"));
        assert_eq!(normalize_codec("XviD").as_deref(), Some("mpeg4"));
    }
}
