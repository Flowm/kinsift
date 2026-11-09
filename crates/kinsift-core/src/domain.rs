//! Domain structs: the user-facing metadata model.
//!
//! Providers fill these structs; the core flattens them into namespaced
//! fields such as `video.height`. Several providers may fill the same
//! domain field; the configured priority decides which value is shown.

use crate::value::Value;

/// A flattened domain struct: `(domain, [(field, value)])`.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldSet {
    pub domain: &'static str,
    pub fields: Vec<(&'static str, Value)>,
}

impl FieldSet {
    pub fn keys(&self) -> impl Iterator<Item = (String, &Value)> {
        self.fields.iter().map(|(k, v)| (format!("{}.{}", self.domain, k), v))
    }
}

/// Implemented by every domain struct.
pub trait Domain {
    const NAME: &'static str;
    fn into_fields(self) -> FieldSet;
}

macro_rules! domain_struct {
    ($(#[$meta:meta])* $name:ident, $domain:literal { $($(#[$fmeta:meta])* $field:ident : $ty:ty => $conv:expr),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq)]
        pub struct $name { $($(#[$fmeta])* pub $field: Option<$ty>,)* }

        impl Domain for $name {
            const NAME: &'static str = $domain;
            #[allow(clippy::redundant_closure_call)]
            fn into_fields(self) -> FieldSet {
                let mut fields = Vec::new();
                $(if let Some(v) = self.$field {
                    fields.push((stringify!($field).trim_start_matches("r#"), ($conv)(v)));
                })*
                FieldSet { domain: $domain, fields }
            }
        }
    };
}

domain_struct!(
    /// File system facts.
    FsMeta, "fs" {
        /// Scan root (absolute).
        root: String => Value::Str,
        /// Path relative to the root, one element per segment.
        path: Vec<String> => Value::Path,
        /// Parent directory relative to the root.
        dir: String => Value::Str,
        name: String => Value::Str,
        /// Lower-case extension without the dot.
        ext: String => Value::Str,
        size: i64 => Value::Int,
        mtime: i64 => Value::Time,
    }
);

domain_struct!(
    /// What the file is: the work, independent of which copy you have.
    /// Files in a duplicate group share these.
    MediaMeta, "media" {
        /// `episode`, `movie` or `extra`.
        r#type: String => Value::Str,
        title: String => Value::Str,
        year: i64 => Value::Int,
        season: i64 => Value::Int,
        /// One or more episode numbers.
        episode: Vec<i64> => |v: Vec<i64>| if v.len() == 1 { Value::Int(v[0]) } else { Value::List(v.into_iter().map(Value::Int).collect()) },
        episode_title: String => Value::Str,
        /// Cut or version of the work: `Director's Cut`, `Extended`.
        edition: String => Value::Str,
        /// Air date, for daily shows.
        date: String => Value::Str,
        /// How sure the source is: `low`, `medium` or `high`.
        confidence: String => Value::Str,
    }
);

domain_struct!(
    /// Which copy you have: the tags of one release. Files in a duplicate
    /// group differ in these.
    ReleaseMeta, "release" {
        group: String => Value::Str,
        /// `Blu-ray`, `Web`, `HDTV`, ...
        source: String => Value::Str,
        /// Resolution tag as written: `1080p`, `2160p`.
        screen_size: String => Value::Str,
        /// Codec tags as written: `x265`, `H.264`, `DTS`.
        video_codec: String => Value::Str,
        audio_codec: String => Value::Str,
    }
);

domain_struct!(
    /// Video stream properties.
    VideoMeta, "video" {
        width: i64 => Value::Int,
        height: i64 => Value::Int,
        /// Resolution class: 4320, 2160, 1080, 720, 576, 480 (from
        /// width and height, so 1920x800 scope encodes count as 1080).
        resolution: i64 => Value::Int,
        /// Normalized codec name: `hevc`, `h264`, `av1`, `vp9`, `mpeg4`, ...
        codec: String => Value::Str,
        fps: f64 => Value::Float,
        /// Overall bit rate in bits per second.
        bitrate: i64 => Value::Int,
        duration: f64 => Value::Duration,
        hdr: String => Value::Str,
    }
);

domain_struct!(
    /// Audio stream properties.
    AudioMeta, "audio" {
        codec: String => Value::Str,
        channels: i64 => Value::Int,
        languages: Vec<String> => |v: Vec<String>| Value::List(v.into_iter().map(Value::Str).collect()),
        tracks: i64 => Value::Int,
    }
);

domain_struct!(
    /// Still image properties.
    ImageMeta, "image" {
        width: i64 => Value::Int,
        height: i64 => Value::Int,
        pixels: i64 => Value::Int,
        format: String => Value::Str,
    }
);

domain_struct!(
    /// Camera metadata.
    ExifMeta, "exif" {
        /// Capture time as recorded by the camera (treated as UTC).
        taken_at: i64 => Value::Time,
        make: String => Value::Str,
        model: String => Value::Str,
        /// `make model`, de-duplicated.
        camera: String => Value::Str,
        lens: String => Value::Str,
        software: String => Value::Str,
        orientation: i64 => Value::Int,
    }
);

domain_struct!(
    /// Content hashes (computed on demand).
    ContentMeta, "content" {
        /// blake3 of the first and last 64 KiB plus the size.
        partial_hash: String => Value::Str,
        /// blake3 of the whole file.
        hash: String => Value::Str,
    }
);

/// Description of a known field, for help output and labels.
pub struct FieldInfo {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

/// Known fields. Providers may emit others; these are the documented ones.
pub const KNOWN_FIELDS: &[FieldInfo] = &[
    FieldInfo { key: "fs.root", label: "", description: "scan root" },
    FieldInfo { key: "fs.path", label: "", description: "path relative to the root (hierarchical)" },
    FieldInfo { key: "fs.dir", label: "", description: "parent directory relative to the root" },
    FieldInfo { key: "fs.name", label: "", description: "file name" },
    FieldInfo { key: "fs.ext", label: "", description: "lower-case extension" },
    FieldInfo { key: "fs.size", label: "", description: "size in bytes" },
    FieldInfo { key: "fs.mtime", label: "", description: "modification time" },
    FieldInfo { key: "media.type", label: "", description: "episode, movie or extra" },
    FieldInfo { key: "media.title", label: "", description: "title of the movie or show" },
    FieldInfo { key: "media.year", label: "", description: "release year" },
    FieldInfo { key: "media.season", label: "Season ", description: "season number" },
    FieldInfo { key: "media.episode", label: "Episode ", description: "episode number(s)" },
    FieldInfo { key: "media.episode_title", label: "", description: "episode title" },
    FieldInfo { key: "media.edition", label: "", description: "cut or version (Director's Cut, Extended)" },
    FieldInfo { key: "release.group", label: "", description: "release group" },
    FieldInfo { key: "release.source", label: "", description: "source (Blu-ray, Web, HDTV)" },
    FieldInfo { key: "release.screen_size", label: "", description: "resolution tag in the name (1080p, 2160p)" },
    FieldInfo { key: "release.video_codec", label: "", description: "video codec tag in the name" },
    FieldInfo { key: "release.audio_codec", label: "", description: "audio codec tag in the name" },
    FieldInfo { key: "video.width", label: "", description: "frame width in pixels" },
    FieldInfo { key: "video.height", label: "", description: "frame height in pixels" },
    FieldInfo { key: "video.resolution", label: "", description: "resolution class (2160, 1080, 720, ...)" },
    FieldInfo { key: "video.codec", label: "", description: "video codec (hevc, h264, av1, ...)" },
    FieldInfo { key: "video.fps", label: "", description: "frame rate" },
    FieldInfo { key: "video.bitrate", label: "", description: "overall bit rate (bit/s)" },
    FieldInfo { key: "video.duration", label: "", description: "duration" },
    FieldInfo { key: "video.hdr", label: "", description: "HDR format" },
    FieldInfo { key: "audio.codec", label: "", description: "first audio codec" },
    FieldInfo { key: "audio.channels", label: "", description: "channels of the first audio track" },
    FieldInfo { key: "audio.languages", label: "", description: "audio track languages" },
    FieldInfo { key: "image.width", label: "", description: "image width" },
    FieldInfo { key: "image.height", label: "", description: "image height" },
    FieldInfo { key: "image.pixels", label: "", description: "width x height" },
    FieldInfo { key: "image.format", label: "", description: "image format" },
    FieldInfo { key: "exif.taken_at", label: "", description: "capture time" },
    FieldInfo { key: "exif.camera", label: "", description: "camera make and model" },
    FieldInfo { key: "exif.software", label: "", description: "editing software, if any" },
    FieldInfo { key: "content.partial_hash", label: "", description: "hash of head and tail (on demand)" },
    FieldInfo { key: "content.hash", label: "", description: "full content hash (on demand)" },
];

/// Resolution class from frame size, width first (so 1920x800 scope encodes
/// count as 1080 and 2560x1080 ultrawide encodes too).
pub fn resolution_class(width: i64, height: i64) -> i64 {
    const CLASSES: [(i64, i64, i64); 5] =
        [(4320, 7000, 4000), (2160, 3200, 2000), (1080, 1800, 1000), (720, 1200, 700), (576, 1000, 560)];
    CLASSES.iter().find(|(_, w, h)| width >= *w || height >= *h).map(|c| c.0).unwrap_or(480)
}

pub fn field_info(key: &str) -> Option<&'static FieldInfo> {
    KNOWN_FIELDS.iter().find(|f| f.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_only_present_fields() {
        let set =
            MediaMeta { title: Some("Show".into()), episode: Some(vec![1, 2]), ..Default::default() }.into_fields();
        let keys: Vec<String> = set.keys().map(|(k, _)| k).collect();
        assert_eq!(keys, ["media.title", "media.episode"]);
        assert_eq!(set.fields[1].1, Value::List(vec![Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn resolution_classes() {
        assert_eq!(resolution_class(1920, 800), 1080);
        assert_eq!(resolution_class(3840, 1600), 2160);
        assert_eq!(resolution_class(1280, 720), 720);
        assert_eq!(resolution_class(720, 576), 576);
        assert_eq!(resolution_class(640, 360), 480);
        assert_eq!(resolution_class(2582, 1080), 1080, "ultrawide 1080p");
        assert_eq!(resolution_class(960, 480), 480);
        assert_eq!(resolution_class(720, 576), 576, "PAL DVD");
    }

    #[test]
    fn raw_identifier_field_name() {
        let set = MediaMeta { r#type: Some("episode".into()), ..Default::default() }.into_fields();
        assert_eq!(set.keys().next().unwrap().0, "media.type");
    }
}
